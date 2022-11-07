/*
 * Copyright (c) 2022 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::env;
use std::fmt;
use std::mem;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use arc_swap::access::Access;
use dns_lookup::lookup_host;
#[cfg(target_os = "linux")]
use flexi_logger::Duplicate;
use flexi_logger::{
    colored_opt_format, Age, Cleanup, Criterion, FileSpec, Logger, LoggerHandle, Naming,
};
use log::{info, warn};
use regex::Regex;

#[cfg(target_os = "linux")]
use crate::ebpf_collector::EbpfCollector;
use crate::handler::{NpbBuilder, PacketHandlerBuilder};
use crate::integration_collector::MetricServer;
use crate::pcap::WorkerManager;
#[cfg(target_os = "linux")]
use crate::platform::ApiWatcher;
#[cfg(target_os = "linux")]
use crate::utils::cgroups::Cgroups;
use crate::{
    collector::Collector,
    collector::{
        flow_aggr::FlowAggrThread, quadruple_generator::QuadrupleGeneratorThread, CollectorThread,
        MetricsType,
    },
    common::{
        enums::TapType, tagged_flow::TaggedFlow, tap_types::TapTyper, DropletMessageType,
        FeatureFlags, DEFAULT_INGESTER_PORT, DEFAULT_LOG_RETENTION, FREE_SPACE_REQUIREMENT,
        NORMAL_EXIT_WITH_RESTART,
    },
    config::{
        handler::{ConfigHandler, DispatcherConfig, ModuleConfig, PortAccess},
        Config, ConfigError, RuntimeConfig, YamlConfig,
    },
    debug::{ConstructDebugCtx, Debugger},
    dispatcher::{
        self, recv_engine::bpf, BpfOptions, Dispatcher, DispatcherBuilder, DispatcherListener,
    },
    exception::ExceptionHandler,
    flow_generator::{AppProtoLogsParser, PacketSequenceParser},
    monitor::Monitor,
    platform::{LibvirtXmlExtractor, PlatformSynchronizer},
    policy::Policy,
    proto::trident::{self, IfMacSource, TapMode},
    rpc::{Session, Synchronizer, DEFAULT_TIMEOUT},
    sender::{uniform_sender::UniformSenderThread, SendItem},
    utils::{
        environment::{
            check, controller_ip_check, free_memory_check, free_space_checker, get_ctrl_ip_and_mac,
            kernel_check, running_in_container, tap_interface_check, trident_process_check,
        },
        guard::Guard,
        logger::{LogLevelWriter, LogWriterAdapter, RemoteLogConfig, RemoteLogWriter},
        stats::{self, Countable, RefCountable, StatsOption},
    },
};
#[cfg(target_os = "linux")]
use public::netns::{links_by_name_regex_in_netns, NetNs};
use public::{
    debug::QueueDebugger,
    netns::NsFile,
    queue,
    utils::net::{get_route_src_ip, links_by_name_regex, MacAddr},
    LeakyBucket,
};

const MINUTE: Duration = Duration::from_secs(60);
const COMMON_DELAY: u32 = 5;

#[derive(Default)]
pub struct ChangedConfig {
    pub runtime_config: RuntimeConfig,
    pub blacklist: Vec<u64>,
    pub vm_mac_addrs: Vec<MacAddr>,
    pub kubernetes_cluster_id: Option<String>,
    pub tap_types: Vec<trident::TapType>,
}

#[derive(Clone, Default, Copy, PartialEq, Eq, Debug)]
pub enum RunningMode {
    #[default]
    Managed,
    Standalone,
}

pub enum State {
    Running,
    ConfigChanged(ChangedConfig),
    Terminated,
    Disabled, // 禁用状态
}

impl State {
    fn unwrap_config(self) -> ChangedConfig {
        match self {
            Self::ConfigChanged(c) => c,
            _ => panic!("not config type"),
        }
    }
}

pub struct VersionInfo {
    pub name: &'static str,
    pub branch: &'static str,
    pub commit_id: &'static str,
    pub rev_count: &'static str,
    pub compiler: &'static str,
    pub compile_time: &'static str,

    pub revision: &'static str,
}

impl fmt::Display for VersionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}
Name: {}
Branch: {}
CommitId: {}
RevCount: {}
Compiler: {}
CompileTime: {}",
            self.rev_count,
            self.commit_id,
            match self.name {
                "deepflow-agent-ce" => "deepflow-agent community edition",
                "deepflow-agent-ee" => "deepflow-agent enterprise edition",
                _ => panic!("unknown deepflow-agent edition"),
            },
            self.branch,
            self.commit_id,
            self.rev_count,
            self.compiler,
            self.compile_time
        )
    }
}

pub type TridentState = Arc<(Mutex<State>, Condvar)>;

pub struct Trident {
    state: TridentState,
    handle: Option<JoinHandle<()>>,
}

#[cfg(unix)]
pub const DEFAULT_TRIDENT_CONF_FILE: &'static str = "/etc/trident.yaml";
#[cfg(windows)]
pub const DEFAULT_TRIDENT_CONF_FILE: &'static str = "C:\\DeepFlow\\trident\\trident-windows.yaml";

impl Trident {
    pub fn start<P: AsRef<Path>>(
        config_path: P,
        version_info: &'static VersionInfo,
        agent_mode: RunningMode,
    ) -> Result<Trident> {
        let config = match agent_mode {
            RunningMode::Managed => {
                match Config::load_from_file(config_path.as_ref()) {
                    Ok(conf) => conf,
                    Err(e) => {
                        if let ConfigError::YamlConfigInvalid(_) = e {
                            // try to load config file from trident.yaml to support upgrading from trident
                            if let Ok(conf) = Config::load_from_file(DEFAULT_TRIDENT_CONF_FILE) {
                                conf
                            } else {
                                // return the original error instead of loading trident conf
                                return Err(e.into());
                            }
                        } else {
                            return Err(e.into());
                        }
                    }
                }
            }
            RunningMode::Standalone => {
                let rc = RuntimeConfig::load_from_file(config_path.as_ref())?;
                let mut conf = Config::default();
                conf.controller_ips = vec!["127.0.0.1".into()];
                conf.log_file = rc.yaml_config.log_file;
                conf.agent_mode = agent_mode;
                conf
            }
        };

        let base_name = Path::new(&env::args().next().unwrap())
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (remote_log_writer, remote_log_config) = RemoteLogWriter::new(
            &config.controller_ips,
            DEFAULT_INGESTER_PORT,
            base_name,
            vec![0, 0, 0, 0, DropletMessageType::Syslog as u8],
        );

        let (log_level_writer, log_level_counter) = LogLevelWriter::new();
        let logger = Logger::try_with_str("info")
            .unwrap()
            .format(colored_opt_format)
            .log_to_file_and_writer(
                FileSpec::try_from(&config.log_file)?,
                Box::new(LogWriterAdapter::new(vec![
                    Box::new(remote_log_writer),
                    Box::new(log_level_writer),
                ])),
            )
            .rotate(
                Criterion::Age(Age::Day),
                Naming::Timestamps,
                Cleanup::KeepLogFiles(DEFAULT_LOG_RETENTION as usize),
            )
            .create_symlink(&config.log_file)
            .append();

        #[cfg(target_os = "linux")]
        let logger = if nix::unistd::getppid().as_raw() != 1 {
            logger.duplicate_to_stderr(Duplicate::All)
        } else {
            logger
        };
        let logger_handle = logger.start()?;

        let stats_collector = Arc::new(stats::Collector::new(&config.controller_ips));
        if matches!(config.agent_mode, RunningMode::Managed) {
            stats_collector.start();
        }

        stats_collector.register_countable(
            "log_counter",
            stats::Countable::Owned(Box::new(log_level_counter)),
            Default::default(),
        );

        info!("static_config {:#?}", config);
        let state = Arc::new((Mutex::new(State::Running), Condvar::new()));
        let state_thread = state.clone();
        let config_path = match agent_mode {
            RunningMode::Managed => None,
            RunningMode::Standalone => Some(config_path.as_ref().to_path_buf()),
        };
        let handle = Some(thread::spawn(move || {
            if let Err(e) = Self::run(
                state_thread,
                config,
                version_info,
                logger_handle,
                remote_log_config,
                stats_collector,
                config_path,
            ) {
                warn!("deepflow-agent exited: {}", e);
                process::exit(1);
            }
        }));

        Ok(Trident { state, handle })
    }

    fn run(
        state: TridentState,
        mut config: Config,
        version_info: &'static VersionInfo,
        logger_handle: LoggerHandle,
        remote_log_config: RemoteLogConfig,
        stats_collector: Arc<stats::Collector>,
        config_path: Option<PathBuf>,
    ) -> Result<()> {
        info!("========== DeepFlow Agent start! ==========");

        let (ctrl_ip, ctrl_mac) = get_ctrl_ip_and_mac(config.controller_ips[0].parse()?);
        if running_in_container() {
            info!(
                "use K8S_NODE_IP_FOR_DEEPFLOW env ip as destination_ip({})",
                ctrl_ip
            );
        }
        info!(
            "agent running in {:?} mode, ctrl_ip {} ctrl_mac {}",
            config.agent_mode, ctrl_ip, ctrl_mac
        );

        let exception_handler = ExceptionHandler::default();
        let session = Arc::new(Session::new(
            config.controller_port,
            config.controller_tls_port,
            DEFAULT_TIMEOUT,
            config.controller_cert_file_prefix.clone(),
            config.controller_ips.clone(),
            exception_handler.clone(),
            &stats_collector,
        ));

        if matches!(config.agent_mode, RunningMode::Managed)
            && running_in_container()
            && config.kubernetes_cluster_id.is_empty()
        {
            config.kubernetes_cluster_id = Config::get_k8s_cluster_id(&session);
            warn!("When running in a K8s pod, the cpu and memory limits notified by deepflow-server will be ignored, please make sure to use K8s for resource limits.");
        }

        let mut config_handler = ConfigHandler::new(
            config,
            ctrl_ip,
            ctrl_mac,
            logger_handle,
            remote_log_config.clone(),
        );

        let synchronizer = Arc::new(Synchronizer::new(
            session.clone(),
            state.clone(),
            version_info,
            ctrl_ip.to_string(),
            ctrl_mac.to_string(),
            config_handler.static_config.controller_ips[0].clone(),
            config_handler.static_config.vtap_group_id_request.clone(),
            config_handler.static_config.kubernetes_cluster_id.clone(),
            exception_handler.clone(),
            config_handler.static_config.agent_mode,
            config_path,
        ));
        stats_collector.register_countable(
            "ntp",
            stats::Countable::Owned(Box::new(synchronizer.ntp_counter())),
            Default::default(),
        );
        synchronizer.start();

        let log_dir = Path::new(config_handler.static_config.log_file.as_str());
        let log_dir = log_dir.parent().unwrap().to_str().unwrap();
        let guard = Guard::new(
            config_handler.environment(),
            log_dir.to_string(),
            exception_handler.clone(),
        );
        guard.start();

        let monitor = Monitor::new(stats_collector.clone(), log_dir.to_string())?;
        monitor.start();

        let (state, cond) = &*state;
        let mut state_guard = state.lock().unwrap();
        let mut components: Option<Components> = None;
        let mut yaml_conf: Option<YamlConfig> = None;

        loop {
            match &*state_guard {
                State::Running => {
                    state_guard = cond.wait(state_guard).unwrap();
                    continue;
                }
                State::Terminated => {
                    if let Some(mut c) = components {
                        c.stop();
                        guard.stop();
                        monitor.stop();
                    }
                    return Ok(());
                }
                State::Disabled => {
                    if let Some(ref mut c) = components {
                        c.stop();
                    }
                    state_guard = cond.wait(state_guard).unwrap();
                    continue;
                }
                _ => (),
            }
            let mut new_state = State::Running;
            mem::swap(&mut new_state, &mut *state_guard);
            mem::drop(state_guard);

            let ChangedConfig {
                runtime_config,
                blacklist,
                vm_mac_addrs,
                kubernetes_cluster_id,
                tap_types,
            } = new_state.unwrap_config();
            if let Some(old_yaml) = yaml_conf {
                if old_yaml != runtime_config.yaml_config {
                    if let Some(mut c) = components.take() {
                        c.stop();
                    }
                    // EbpfCollector does not support recreation because it calls bpf_tracer_init, which can only be called once in a process
                    // Work around this problem by exiting and restart trident
                    warn!("yaml_config updated, agent restart...");
                    thread::sleep(Duration::from_secs(1));
                    process::exit(NORMAL_EXIT_WITH_RESTART);
                }
            }
            yaml_conf = Some(runtime_config.yaml_config.clone());
            if let Some(id) = kubernetes_cluster_id {
                config_handler.static_config.kubernetes_cluster_id = id;
            }
            let callbacks =
                config_handler.on_config(runtime_config, &exception_handler, components.as_mut());
            match components.as_mut() {
                None => {
                    let mut comp = Components::new(
                        &config_handler,
                        stats_collector.clone(),
                        &session,
                        &synchronizer,
                        exception_handler.clone(),
                        remote_log_config.clone(),
                        vm_mac_addrs,
                        config_handler.static_config.agent_mode,
                    )?;
                    comp.start();
                    if config_handler.candidate_config.dispatcher.tap_mode == TapMode::Analyzer {
                        parse_tap_type(&mut comp, tap_types);
                    }
                    for callback in callbacks {
                        callback(&config_handler, &mut comp);
                    }
                    components.replace(comp);
                }
                Some(mut components) => {
                    components.start();
                    components.config = config_handler.candidate_config.clone();
                    dispatcher_listener_callback(
                        &config_handler.candidate_config.dispatcher,
                        &mut components,
                        blacklist,
                        vm_mac_addrs,
                        tap_types,
                    );
                    for callback in callbacks {
                        callback(&config_handler, components);
                    }
                    for listener in components.dispatcher_listeners.iter_mut() {
                        listener.on_config_change(&config_handler.candidate_config.dispatcher);
                    }
                }
            }
            state_guard = state.lock().unwrap();
        }
    }

    pub fn stop(&mut self) {
        info!("Gracefully stopping");
        let (state, cond) = &*self.state;

        let mut state_guard = state.lock().unwrap();
        *state_guard = State::Terminated;
        cond.notify_one();
        mem::drop(state_guard);
        self.handle.take().unwrap().join().unwrap();
        info!("Gracefully stopped");
    }
}

fn dispatcher_listener_callback(
    conf: &DispatcherConfig,
    components: &mut Components,
    blacklist: Vec<u64>,
    vm_mac_addrs: Vec<MacAddr>,
    tap_types: Vec<trident::TapType>,
) {
    match conf.tap_mode {
        TapMode::Local => {
            let if_mac_source = conf.if_mac_source;
            let links = match links_by_name_regex(&conf.tap_interface_regex) {
                Err(e) => {
                    warn!("get interfaces by name regex failed: {}", e);
                    vec![]
                }
                Ok(links) => {
                    if links.is_empty() {
                        warn!(
                            "tap-interface-regex({}) do not match any interface, in local mode",
                            conf.tap_interface_regex
                        );
                    }
                    links
                }
            };
            for listener in components.dispatcher_listeners.iter() {
                #[cfg(target_os = "linux")]
                let netns = listener.netns();
                #[cfg(target_os = "linux")]
                if netns != NsFile::Root {
                    let interfaces = match links_by_name_regex_in_netns(
                        &conf.tap_interface_regex,
                        &netns,
                    ) {
                        Err(e) => {
                            warn!("get interfaces by name regex in {:?} failed: {}", netns, e);
                            vec![]
                        }
                        Ok(links) => {
                            if links.is_empty() {
                                warn!(
                                    "tap-interface-regex({}) do not match any interface in {:?}, in local mode",
                                    conf.tap_interface_regex, netns,
                                );
                            }
                            links
                        }
                    };
                    info!("tap interface in namespace {:?}: {:?}", netns, interfaces);
                    listener.on_tap_interface_change(
                        &interfaces,
                        if_mac_source,
                        conf.trident_type,
                        &blacklist,
                    );
                    continue;
                }
                listener.on_tap_interface_change(
                    &links,
                    if_mac_source,
                    conf.trident_type,
                    &blacklist,
                );
                listener.on_vm_change(&vm_mac_addrs);
            }
        }
        TapMode::Mirror => {
            for listener in components.dispatcher_listeners.iter() {
                listener.on_tap_interface_change(
                    &vec![],
                    IfMacSource::IfMac,
                    conf.trident_type,
                    &blacklist,
                );
                listener.on_vm_change(&vm_mac_addrs);
            }
        }
        TapMode::Analyzer => {
            for listener in components.dispatcher_listeners.iter() {
                listener.on_tap_interface_change(
                    &vec![],
                    IfMacSource::IfMac,
                    conf.trident_type,
                    &blacklist,
                );
                listener.on_vm_change(&vm_mac_addrs);
            }
            parse_tap_type(components, tap_types);
        }
        _ => {}
    }
}

fn parse_tap_type(components: &mut Components, tap_types: Vec<trident::TapType>) {
    let mut updated = false;
    if components.cur_tap_types.len() != tap_types.len() {
        updated = true;
    } else {
        for i in 0..tap_types.len() {
            if components.cur_tap_types[i] != tap_types[i] {
                updated = true;
                break;
            }
        }
    }
    if updated {
        components.tap_typer.on_tap_types_change(tap_types.clone());
        components.cur_tap_types.clear();
        components.cur_tap_types.clone_from(&tap_types);
    }
}

pub struct DomainNameListener {
    stats_collector: Arc<stats::Collector>,
    synchronizer: Arc<Synchronizer>,
    remote_log_config: RemoteLogConfig,

    ips: Vec<String>,
    domain_names: Vec<String>,
    port_config: PortAccess,

    thread_handler: Option<JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}

impl DomainNameListener {
    const INTERVAL: u64 = 5;

    fn new(
        stats_collector: Arc<stats::Collector>,
        synchronizer: Arc<Synchronizer>,
        remote_log_config: RemoteLogConfig,
        domain_names: Vec<String>,
        ips: Vec<String>,
        port_config: PortAccess,
    ) -> DomainNameListener {
        Self {
            stats_collector: stats_collector.clone(),
            synchronizer: synchronizer.clone(),
            remote_log_config,

            domain_names: domain_names.clone(),
            ips: ips.clone(),
            port_config,

            thread_handler: None,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(&mut self) {
        if self.thread_handler.is_some() {
            return;
        }
        self.stopped.store(false, Ordering::Relaxed);
        self.run();
    }

    fn stop(&mut self) {
        if self.thread_handler.is_none() {
            return;
        }
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(handler) = self.thread_handler.take() {
            let _ = handler.join();
        }
    }

    fn run(&mut self) {
        if self.domain_names.len() == 0 {
            return;
        }
        let stats_collector = self.stats_collector.clone();
        let synchronizer = self.synchronizer.clone();

        let mut ips = self.ips.clone();
        let domain_names = self.domain_names.clone();
        let stopped = self.stopped.clone();
        let remote_log_config = self.remote_log_config.clone();
        let port_config = self.port_config.clone();

        info!(
            "Resolve controller domain name {} {}",
            domain_names[0], ips[0]
        );

        self.thread_handler = Some(thread::spawn(move || {
            while !stopped.swap(false, Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(Self::INTERVAL));

                let mut changed = false;
                for i in 0..domain_names.len() {
                    let current = lookup_host(domain_names[i].as_str());
                    if current.is_err() {
                        continue;
                    }
                    let current = current.unwrap();

                    changed = current.iter().find(|&&x| x.to_string() == ips[i]).is_none();
                    if changed {
                        info!(
                            "Domain name {} ip {} change to {}",
                            domain_names[i], ips[i], current[0]
                        );
                        ips[i] = current[0].to_string();
                    }
                }

                if changed {
                    let (ctrl_ip, ctrl_mac) = get_ctrl_ip_and_mac(ips[0].parse().unwrap());
                    info!(
                        "use K8S_NODE_IP_FOR_DEEPFLOW env ip as destination_ip({})",
                        ctrl_ip
                    );

                    synchronizer.reset_session(
                        ips.clone(),
                        ctrl_ip.to_string(),
                        ctrl_mac.to_string(),
                    );
                    stats_collector.set_remotes(
                        ips.iter()
                            .map(|item| item.parse::<IpAddr>().unwrap())
                            .collect(),
                    );

                    remote_log_config.set_remotes(&ips, port_config.load().analyzer_port);
                }
            }
        }));
    }
}

pub struct Components {
    pub config: ModuleConfig,
    pub rx_leaky_bucket: Arc<LeakyBucket>,
    pub l7_log_rate: Arc<LeakyBucket>,
    pub libvirt_xml_extractor: Arc<LibvirtXmlExtractor>,
    pub tap_typer: Arc<TapTyper>,
    pub cur_tap_types: Vec<trident::TapType>,
    pub dispatchers: Vec<Dispatcher>,
    pub dispatcher_listeners: Vec<DispatcherListener>,
    pub log_parsers: Vec<AppProtoLogsParser>,
    pub collectors: Vec<CollectorThread>,
    pub l4_flow_uniform_sender: UniformSenderThread,
    pub metrics_uniform_sender: UniformSenderThread,
    pub l7_flow_uniform_sender: UniformSenderThread,
    pub stats_sender: UniformSenderThread,
    pub platform_synchronizer: PlatformSynchronizer,
    #[cfg(target_os = "linux")]
    pub api_watcher: Arc<ApiWatcher>,
    pub debugger: Debugger,
    pub pcap_manager: WorkerManager,
    #[cfg(target_os = "linux")]
    pub ebpf_collector: Option<Box<EbpfCollector>>,
    pub running: AtomicBool,
    pub stats_collector: Arc<stats::Collector>,
    #[cfg(target_os = "linux")]
    pub cgroups_controller: Arc<Cgroups>,
    pub external_metrics_server: MetricServer,
    pub otel_uniform_sender: UniformSenderThread,
    pub prometheus_uniform_sender: UniformSenderThread,
    pub telegraf_uniform_sender: UniformSenderThread,
    pub packet_sequence_parsers: Vec<PacketSequenceParser>, // Enterprise Edition Feature: packet-sequence
    pub packet_sequence_uniform_sender: UniformSenderThread, // Enterprise Edition Feature: packet-sequence
    pub exception_handler: ExceptionHandler,
    pub domain_name_listener: DomainNameListener,
    pub npb_bps_limit: Arc<LeakyBucket>,
    pub handler_builders: Vec<Arc<Mutex<Vec<PacketHandlerBuilder>>>>,
    pub compressed_otel_uniform_sender: UniformSenderThread,
    max_memory: u64,
    tap_mode: TapMode,
    agent_mode: RunningMode,
}

impl Components {
    fn start(&mut self) {
        if self.running.swap(true, Ordering::Relaxed) {
            return;
        }
        info!("Staring components.");
        self.libvirt_xml_extractor.start();
        self.pcap_manager.start();
        if matches!(self.agent_mode, RunningMode::Managed) {
            self.platform_synchronizer.start();
            #[cfg(target_os = "linux")]
            self.api_watcher.start();
        }
        #[cfg(target_os = "linux")]
        self.platform_synchronizer.start_kubernetes_poller();
        self.debugger.start();
        self.metrics_uniform_sender.start();
        self.l7_flow_uniform_sender.start();
        self.l4_flow_uniform_sender.start();

        // Enterprise Edition Feature: packet-sequence
        self.packet_sequence_uniform_sender.start();
        for packet_sequence_parser in self.packet_sequence_parsers.iter() {
            packet_sequence_parser.start();
        }

        if self.tap_mode != TapMode::Analyzer
            && self.config.platform.kubernetes_cluster_id.is_empty()
        {
            match free_memory_check(self.max_memory, &self.exception_handler) {
                Ok(()) => {
                    for dispatcher in self.dispatchers.iter() {
                        dispatcher.start();
                    }
                }
                Err(e) => {
                    warn!("{}", e);
                }
            }
        } else {
            for dispatcher in self.dispatchers.iter() {
                dispatcher.start();
            }
        }

        for log_parser in self.log_parsers.iter() {
            log_parser.start();
        }

        for collector in self.collectors.iter_mut() {
            collector.start();
        }

        #[cfg(target_os = "linux")]
        if let Some(ebpf_collector) = self.ebpf_collector.as_mut() {
            ebpf_collector.start();
        }
        if matches!(self.agent_mode, RunningMode::Managed) {
            self.otel_uniform_sender.start();
            self.compressed_otel_uniform_sender.start();
            self.prometheus_uniform_sender.start();
            self.telegraf_uniform_sender.start();
            if self.config.metric_server.enabled {
                self.external_metrics_server.start();
            }
        }
        self.domain_name_listener.start();
        self.handler_builders.iter().for_each(|x| {
            x.lock().unwrap().iter_mut().for_each(|y| {
                y.start();
            })
        });
        info!("Started components.");
    }

    fn new(
        config_handler: &ConfigHandler,
        stats_collector: Arc<stats::Collector>,
        session: &Arc<Session>,
        synchronizer: &Arc<Synchronizer>,
        exception_handler: ExceptionHandler,
        remote_log_config: RemoteLogConfig,
        vm_mac_addrs: Vec<MacAddr>,
        agent_mode: RunningMode,
    ) -> Result<Self> {
        let static_config = &config_handler.static_config;
        let candidate_config = &config_handler.candidate_config;
        let yaml_config = &candidate_config.yaml_config;
        let ctrl_ip = config_handler.ctrl_ip;
        let ctrl_mac = config_handler.ctrl_mac;
        let max_memory = config_handler.candidate_config.environment.max_memory;

        let mut stats_sender = UniformSenderThread::new(
            stats::DFSTATS_SENDER_ID,
            "stats",
            stats_collector.get_receiver(),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );
        stats_sender.start();

        trident_process_check();
        controller_ip_check(&static_config.controller_ips);
        check(free_space_checker(
            &static_config.log_file,
            FREE_SPACE_REQUIREMENT,
            exception_handler.clone(),
        ));

        match candidate_config.tap_mode {
            TapMode::Analyzer => {
                kernel_check();
                tap_interface_check(&yaml_config.src_interfaces);
            }
            _ => {
                // NPF服务检查
                // TODO: npf (only on windows)
                if candidate_config.tap_mode == TapMode::Mirror {
                    kernel_check();
                }
            }
        }

        info!(
            "Agent run with feature-flags: {:?}.",
            FeatureFlags::from(&yaml_config.feature_flags)
        );
        // Currently, only loca-mode + ebpf collector is supported, and ebpf collector is not
        // applicable to fastpath, so the number of queues is 1
        // =================================================================================
        // 目前仅支持local-mode + ebpf-collector，ebpf-collector不适用fastpath, 所以队列数为1
        let (policy_setter, policy_getter) = Policy::new(
            1.max(yaml_config.src_interfaces.len()),
            yaml_config.first_path_level as usize,
            yaml_config.fast_path_map_size,
            false,
            FeatureFlags::from(&yaml_config.feature_flags),
        );
        synchronizer.add_flow_acl_listener(Box::new(policy_setter));
        // TODO: collector enabled
        // TODO: packet handler builders

        let libvirt_xml_extractor = Arc::new(LibvirtXmlExtractor::new());
        #[cfg(target_os = "linux")]
        let platform_synchronizer = PlatformSynchronizer::new(
            config_handler.platform(),
            session.clone(),
            libvirt_xml_extractor.clone(),
            exception_handler.clone(),
            candidate_config.dispatcher.extra_netns_regex.clone(),
        );
        #[cfg(target_os = "windows")]
        let platform_synchronizer = PlatformSynchronizer::new(
            config_handler.platform(),
            session.clone(),
            exception_handler.clone(),
        );

        #[cfg(target_os = "linux")]
        let api_watcher = Arc::new(ApiWatcher::new(
            config_handler.platform(),
            session.clone(),
            exception_handler.clone(),
        ));

        let context = ConstructDebugCtx {
            #[cfg(target_os = "linux")]
            api_watcher: api_watcher.clone(),
            #[cfg(target_os = "linux")]
            poller: platform_synchronizer.clone_poller(),
            session: session.clone(),
            static_config: synchronizer.static_config.clone(),
            running_config: synchronizer.running_config.clone(),
            status: synchronizer.status.clone(),
            config: config_handler.debug(),
            policy_setter,
        };
        let debugger = Debugger::new(context);
        let queue_debugger = debugger.clone_queue();

        let (pcap_sender, pcap_receiver, _) = queue::bounded_with_debug(
            config_handler.candidate_config.pcap.queue_size as usize,
            "1-mini-meta-packet-to-pcap",
            &queue_debugger,
        );

        let pcap_manager = WorkerManager::new(
            config_handler.pcap(),
            vec![pcap_receiver],
            stats_collector.clone(),
            synchronizer.ntp_diff(),
        );

        let rx_leaky_bucket = Arc::new(LeakyBucket::new(match candidate_config.tap_mode {
            TapMode::Analyzer => None,
            _ => Some(
                config_handler
                    .candidate_config
                    .dispatcher
                    .global_pps_threshold,
            ),
        }));

        let tap_typer = Arc::new(TapTyper::new());

        let tap_interfaces = match links_by_name_regex(
            &config_handler
                .candidate_config
                .dispatcher
                .tap_interface_regex,
        ) {
            Err(e) => {
                warn!("get interfaces by name regex failed: {}", e);
                vec![]
            }
            Ok(links) if links.is_empty() => {
                warn!(
                    "tap-interface-regex({}) do not match any interface, in local mode",
                    config_handler
                        .candidate_config
                        .dispatcher
                        .tap_interface_regex
                );
                vec![]
            }
            Ok(links) => links,
        };

        // TODO: collector enabled
        let mut dispatchers = vec![];
        let mut dispatcher_listeners = vec![];
        let mut collectors = vec![];
        let mut log_parsers = vec![];
        let mut packet_sequence_parsers = vec![]; // Enterprise Edition Feature: packet-sequence

        // Sender/Collector
        info!(
            "static analyzer ip: {} actual analyzer ip {}",
            yaml_config.analyzer_ip, candidate_config.sender.dest_ip
        );
        let sender_id = 0usize;
        let l4_flow_aggr_queue_name = "3-flow-to-collector-sender";
        let (l4_flow_aggr_sender, l4_flow_aggr_receiver, counter) = queue::bounded_with_debug(
            yaml_config.flow_sender_queue_size as usize,
            l4_flow_aggr_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", l4_flow_aggr_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let l4_flow_uniform_sender = UniformSenderThread::new(
            sender_id,
            l4_flow_aggr_queue_name,
            Arc::new(l4_flow_aggr_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let sender_id = 1usize;
        let metrics_queue_name = "2-doc-to-collector-sender";
        let (metrics_sender, metrics_receiver, counter) = queue::bounded_with_debug(
            yaml_config.collector_sender_queue_size,
            metrics_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", metrics_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let metrics_uniform_sender = UniformSenderThread::new(
            sender_id,
            metrics_queue_name,
            Arc::new(metrics_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let sender_id = 2usize;
        let proto_log_queue_name = "3-protolog-to-collector-sender";
        let (proto_log_sender, proto_log_receiver, counter) = queue::bounded_with_debug(
            yaml_config.flow_sender_queue_size,
            proto_log_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", proto_log_queue_name.to_string()),
                StatsOption::Tag("index", "0".to_string()),
            ],
        );
        let l7_flow_uniform_sender = UniformSenderThread::new(
            sender_id,
            proto_log_queue_name,
            Arc::new(proto_log_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        // Dispatcher
        let source_ip = match get_route_src_ip(&candidate_config.dispatcher.analyzer_ip) {
            Ok(ip) => ip,
            Err(e) => {
                warn!(
                    "get route to {} failed: {:?}",
                    candidate_config.dispatcher.analyzer_ip, e
                );
                Ipv4Addr::UNSPECIFIED.into()
            }
        };
        let bpf_builder = bpf::Builder {
            is_ipv6: ctrl_ip.is_ipv6(),
            vxlan_flags: yaml_config.vxlan_flags,
            vxlan_port: yaml_config.vxlan_port,
            controller_port: static_config.controller_port,
            controller_tls_port: static_config.controller_tls_port,
            proxy_controller_port: candidate_config.dispatcher.proxy_controller_port,
            analyzer_source_ip: source_ip,
            analyzer_port: candidate_config.dispatcher.analyzer_port,
        };
        #[cfg(target_os = "linux")]
        let bpf_syntax = bpf_builder.build_pcap_syntax();
        #[cfg(target_os = "windows")]
        let bpf_syntax_str = bpf_builder.build_pcap_syntax_to_str();

        let l7_log_rate = Arc::new(LeakyBucket::new(Some(
            candidate_config.log_parser.l7_log_collect_nps_threshold,
        )));

        // Enterprise Edition Feature: packet-sequence
        let sender_id = 6; // TODO sender_id should be generated automatically
        let packet_sequence_queue_name = "packet_sequence_block-to-sender";
        let (packet_sequence_uniform_output, packet_sequence_uniform_input, counter) =
            queue::bounded_with_debug(
                yaml_config.packet_sequence_queue_size,
                packet_sequence_queue_name,
                &queue_debugger,
            );

        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", packet_sequence_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let packet_sequence_uniform_sender = UniformSenderThread::new(
            sender_id,
            packet_sequence_queue_name,
            Arc::new(packet_sequence_uniform_input),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let bpf_options = Arc::new(Mutex::new(BpfOptions {
            capture_bpf: candidate_config.dispatcher.capture_bpf.clone(),
            #[cfg(target_os = "linux")]
            bpf_syntax,
            #[cfg(target_os = "windows")]
            bpf_syntax_str,
        }));

        let npb_bps_limit = Arc::new(LeakyBucket::new(Some(
            config_handler.candidate_config.npb.bps_threshold,
        )));
        let mut handler_builders = Vec::new();

        let mut src_interfaces_and_namespaces = vec![];
        for src_if in yaml_config.src_interfaces.iter() {
            src_interfaces_and_namespaces.push((src_if.clone(), NsFile::Root));
        }
        if src_interfaces_and_namespaces.is_empty() {
            src_interfaces_and_namespaces.push(("".into(), NsFile::Root));
        }
        #[cfg(target_os = "linux")]
        if candidate_config.dispatcher.extra_netns_regex != "" {
            let re = Regex::new(&candidate_config.dispatcher.extra_netns_regex).unwrap();
            let mut nss = NetNs::find_ns_files_by_regex(&re);
            nss.sort_unstable();
            for ns in nss {
                src_interfaces_and_namespaces.push(("".into(), ns));
            }
        }

        for (i, (src_interface, netns)) in src_interfaces_and_namespaces.into_iter().enumerate() {
            let (flow_sender, flow_receiver, counter) = queue::bounded_with_debug(
                yaml_config.flow_queue_size,
                "1-tagged-flow-to-quadruple-generator",
                &queue_debugger,
            );
            stats_collector.register_countable(
                "queue",
                Countable::Owned(Box::new(counter)),
                vec![
                    StatsOption::Tag("module", "1-tagged-flow-to-quadruple-generator".to_string()),
                    StatsOption::Tag("index", i.to_string()),
                ],
            );

            // create and start app proto logs
            let (log_sender, log_receiver, counter) = queue::bounded_with_debug(
                yaml_config.flow_queue_size,
                "1-tagged-flow-to-app-protocol-logs",
                &queue_debugger,
            );
            stats_collector.register_countable(
                "queue",
                Countable::Owned(Box::new(counter)),
                vec![
                    StatsOption::Tag("module", "1-tagged-flow-to-app-protocol-logs".to_string()),
                    StatsOption::Tag("index", i.to_string()),
                ],
            );

            let (app_proto_log_parser, counter) = AppProtoLogsParser::new(
                log_receiver,
                proto_log_sender.clone(),
                i as u32,
                config_handler.log_parser(),
                l7_log_rate.clone(),
            );
            stats_collector.register_countable(
                "l7_session_aggr",
                Countable::Ref(Arc::downgrade(&counter) as Weak<dyn RefCountable>),
                vec![StatsOption::Tag("index", i.to_string())],
            );
            log_parsers.push(app_proto_log_parser);

            // Enterprise Edition Feature: packet-sequence
            // create and start packet sequence
            let (packet_sequence_sender, packet_sequence_receiver, counter) =
                queue::bounded_with_debug(
                    yaml_config.packet_sequence_queue_size,
                    "1-packet-sequence-block-to-uniform-collect-sender",
                    &queue_debugger,
                );
            stats_collector.register_countable(
                "queue",
                Countable::Owned(Box::new(counter)),
                vec![
                    StatsOption::Tag(
                        "module",
                        "1-packet-sequence-block-to-uniform-collect-sender".to_string(),
                    ),
                    StatsOption::Tag("index", i.to_string()),
                ],
            );

            let packet_sequence_parser = PacketSequenceParser::new(
                packet_sequence_receiver,
                packet_sequence_uniform_output.clone(),
                i as u32,
            );
            packet_sequence_parsers.push(packet_sequence_parser);

            let handler_builder = Arc::new(Mutex::new(vec![
                PacketHandlerBuilder::Pcap(pcap_sender.clone()),
                PacketHandlerBuilder::Npb(NpbBuilder::new(
                    i,
                    &config_handler.candidate_config.npb,
                    &queue_debugger,
                    npb_bps_limit.clone(),
                    stats_collector.clone(),
                )),
            ]));
            handler_builders.push(handler_builder.clone());

            #[cfg(target_os = "linux")]
            let tap_interfaces = if netns != NsFile::Root {
                let interfaces = match links_by_name_regex_in_netns(
                    &config_handler
                        .candidate_config
                        .dispatcher
                        .tap_interface_regex,
                    &netns,
                ) {
                    Err(e) => {
                        warn!("get interfaces by name regex in {:?} failed: {}", netns, e);
                        vec![]
                    }
                    Ok(links) => {
                        if links.is_empty() {
                            warn!(
                                "tap-interface-regex({}) do not match any interface in {:?}, in local mode",
                                config_handler.candidate_config.dispatcher.tap_interface_regex, netns,
                            );
                        }
                        links
                    }
                };
                info!("tap interface in namespace {:?}: {:?}", netns, interfaces);
                interfaces
            } else {
                tap_interfaces.clone()
            };

            let dispatcher_builder = DispatcherBuilder::new()
                .id(i)
                .handler_builders(handler_builder)
                .ctrl_mac(ctrl_mac)
                .leaky_bucket(rx_leaky_bucket.clone())
                .options(Arc::new(dispatcher::Options {
                    #[cfg(target_os = "linux")]
                    af_packet_blocks: config_handler.candidate_config.dispatcher.af_packet_blocks,
                    #[cfg(target_os = "linux")]
                    af_packet_version: config_handler.candidate_config.dispatcher.af_packet_version,
                    #[cfg(target_os = "windows")]
                    win_packet_blocks: config_handler.candidate_config.dispatcher.af_packet_blocks,
                    tap_mode: candidate_config.tap_mode,
                    tap_mac_script: yaml_config.tap_mac_script.clone(),
                    is_ipv6: ctrl_ip.is_ipv6(),
                    vxlan_port: yaml_config.vxlan_port,
                    vxlan_flags: yaml_config.vxlan_flags,
                    controller_port: static_config.controller_port,
                    controller_tls_port: static_config.controller_tls_port,
                    snap_len: config_handler
                        .candidate_config
                        .dispatcher
                        .capture_packet_size as usize,
                    ..Default::default()
                }))
                .bpf_options(bpf_options.clone())
                .default_tap_type(
                    (yaml_config.default_tap_type as u16)
                        .try_into()
                        .unwrap_or(TapType::Cloud),
                )
                .mirror_traffic_pcp(yaml_config.mirror_traffic_pcp)
                .tap_typer(tap_typer.clone())
                .analyzer_dedup_disabled(yaml_config.analyzer_dedup_disabled)
                .libvirt_xml_extractor(libvirt_xml_extractor.clone())
                .flow_output_queue(flow_sender)
                .log_output_queue(log_sender)
                .packet_sequence_output_queue(packet_sequence_sender) // Enterprise Edition Feature: packet-sequence
                .stats_collector(stats_collector.clone())
                .flow_map_config(config_handler.flow())
                .log_parse_config(config_handler.log_parser())
                .policy_getter(policy_getter)
                .exception_handler(exception_handler.clone())
                .ntp_diff(synchronizer.ntp_diff())
                .src_interface(src_interface)
                .netns(netns)
                .trident_type(candidate_config.dispatcher.trident_type);

            #[cfg(target_os = "linux")]
            let dispatcher = dispatcher_builder
                .platform_poller(platform_synchronizer.clone_poller())
                .build()
                .unwrap();
            #[cfg(target_os = "windows")]
            let dispatcher = dispatcher_builder
                .pcap_interfaces(tap_interfaces.clone())
                .build()
                .unwrap();

            let mut dispatcher_listener = dispatcher.listener();
            dispatcher_listener.on_config_change(&candidate_config.dispatcher);
            dispatcher_listener.on_tap_interface_change(
                &tap_interfaces,
                candidate_config.dispatcher.if_mac_source,
                candidate_config.dispatcher.trident_type,
                &vec![],
            );
            dispatcher_listener.on_vm_change(&vm_mac_addrs);

            dispatchers.push(dispatcher);
            dispatcher_listeners.push(dispatcher_listener);

            // create and start collector
            let collector = Self::new_collector(
                i,
                stats_collector.clone(),
                flow_receiver,
                l4_flow_aggr_sender.clone(),
                metrics_sender.clone(),
                MetricsType::SECOND | MetricsType::MINUTE,
                config_handler,
                &queue_debugger,
                &synchronizer,
            );
            collectors.push(collector);
        }

        #[cfg(target_os = "linux")]
        let ebpf_collector = EbpfCollector::new(
            synchronizer.ntp_diff(),
            &config_handler.candidate_config.ebpf,
            config_handler.log_parser(),
            policy_getter,
            l7_log_rate.clone(),
            proto_log_sender,
            &queue_debugger,
        )
        .ok();
        #[cfg(target_os = "linux")]
        if let Some(collector) = &ebpf_collector {
            stats_collector.register_countable(
                "ebpf-collector",
                Countable::Owned(Box::new(collector.get_sync_counter())),
                vec![],
            );
        }
        #[cfg(target_os = "linux")]
        let cgroups_controller: Arc<Cgroups> = Arc::new(Cgroups { cgroup: None });

        let sender_id = 3;
        let otel_queue_name = "otel-to-sender";
        let (otel_sender, otel_receiver, counter) = queue::bounded_with_debug(
            yaml_config.external_metrics_sender_queue_size,
            otel_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", otel_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let otel_uniform_sender = UniformSenderThread::new(
            sender_id,
            otel_queue_name,
            Arc::new(otel_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let sender_id = 4;
        let prometheus_queue_name = "prometheus-to-sender";
        let (prometheus_sender, prometheus_receiver, counter) = queue::bounded_with_debug(
            yaml_config.external_metrics_sender_queue_size,
            prometheus_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", prometheus_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let prometheus_uniform_sender = UniformSenderThread::new(
            sender_id,
            prometheus_queue_name,
            Arc::new(prometheus_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let sender_id = 5;
        let telegraf_queue_name = "telegraf-to-sender";
        let (telegraf_sender, telegraf_receiver, counter) = queue::bounded_with_debug(
            yaml_config.external_metrics_sender_queue_size,
            telegraf_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", telegraf_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let telegraf_uniform_sender = UniformSenderThread::new(
            sender_id,
            telegraf_queue_name,
            Arc::new(telegraf_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let sender_id = 6;
        let compressed_otel_queue_name = "compressed-otel-to-sender";
        let (compressed_otel_sender, compressed_otel_receiver, counter) = queue::bounded_with_debug(
            yaml_config.external_metrics_sender_queue_size,
            compressed_otel_queue_name,
            &queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", compressed_otel_queue_name.to_string()),
                StatsOption::Tag("index", sender_id.to_string()),
            ],
        );
        let compressed_otel_uniform_sender = UniformSenderThread::new(
            sender_id,
            compressed_otel_queue_name,
            Arc::new(compressed_otel_receiver),
            config_handler.sender(),
            stats_collector.clone(),
            exception_handler.clone(),
        );

        let (external_metrics_server, external_metrics_counter) = MetricServer::new(
            otel_sender,
            compressed_otel_sender,
            prometheus_sender,
            telegraf_sender,
            candidate_config.metric_server.port,
            exception_handler.clone(),
            candidate_config.metric_server.compressed,
        );

        stats_collector.register_countable(
            "integration_collector",
            Countable::Owned(Box::new(external_metrics_counter)),
            Default::default(),
        );

        remote_log_config.set_enabled(candidate_config.log.rsyslog_enabled);
        remote_log_config.set_threshold(candidate_config.log.log_threshold);
        remote_log_config.set_hostname(candidate_config.log.host.clone());

        let domain_name_listener = DomainNameListener::new(
            stats_collector.clone(),
            synchronizer.clone(),
            remote_log_config,
            config_handler.static_config.controller_domain_name.clone(),
            config_handler.static_config.controller_ips.clone(),
            config_handler.port(),
        );

        Ok(Components {
            config: candidate_config.clone(),
            rx_leaky_bucket,
            l7_log_rate,
            libvirt_xml_extractor,
            tap_typer,
            cur_tap_types: vec![],
            dispatchers,
            dispatcher_listeners,
            collectors,
            l4_flow_uniform_sender,
            metrics_uniform_sender,
            l7_flow_uniform_sender,
            stats_sender,
            platform_synchronizer,
            #[cfg(target_os = "linux")]
            api_watcher,
            debugger,
            pcap_manager,
            log_parsers,
            #[cfg(target_os = "linux")]
            ebpf_collector,
            stats_collector,
            running: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            cgroups_controller,
            external_metrics_server,
            exception_handler,
            max_memory,
            otel_uniform_sender,
            prometheus_uniform_sender,
            telegraf_uniform_sender,
            tap_mode: candidate_config.tap_mode,
            packet_sequence_uniform_sender, // Enterprise Edition Feature: packet-sequence
            packet_sequence_parsers,        // Enterprise Edition Feature: packet-sequence
            domain_name_listener,
            npb_bps_limit,
            handler_builders,
            compressed_otel_uniform_sender,
            agent_mode,
        })
    }

    fn new_collector(
        id: usize,
        stats_collector: Arc<stats::Collector>,
        flow_receiver: queue::Receiver<Box<TaggedFlow>>,
        l4_flow_aggr_sender: queue::DebugSender<SendItem>,
        metrics_sender: queue::DebugSender<SendItem>,
        metrics_type: MetricsType,
        config_handler: &ConfigHandler,
        queue_debugger: &QueueDebugger,
        synchronizer: &Arc<Synchronizer>,
    ) -> CollectorThread {
        let yaml_config = &config_handler.candidate_config.yaml_config;
        let (second_sender, second_receiver, counter) = queue::bounded_with_debug(
            yaml_config.quadruple_queue_size,
            "2-flow-with-meter-to-second-collector",
            queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag(
                    "module",
                    "2-flow-with-meter-to-second-collector".to_string(),
                ),
                StatsOption::Tag("index", id.to_string()),
            ],
        );
        let (minute_sender, minute_receiver, counter) = queue::bounded_with_debug(
            yaml_config.quadruple_queue_size,
            "2-flow-with-meter-to-minute-collector",
            queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag(
                    "module",
                    "2-flow-with-meter-to-minute-collector".to_string(),
                ),
                StatsOption::Tag("index", id.to_string()),
            ],
        );

        let (l4_log_sender, l4_log_receiver, counter) = queue::bounded_with_debug(
            yaml_config.flow.aggr_queue_size as usize,
            "2-second-flow-to-minute-aggrer",
            queue_debugger,
        );
        stats_collector.register_countable(
            "queue",
            Countable::Owned(Box::new(counter)),
            vec![
                StatsOption::Tag("module", "2-second-flow-to-minute-aggrer".to_string()),
                StatsOption::Tag("index", id.to_string()),
            ],
        );

        // FIXME: 应该让flowgenerator和dispatcher解耦，并提供Delay函数用于此处
        // QuadrupleGenerator的Delay组成部分：
        //   FlowGen中流统计数据固有的Delay：_FLOW_STAT_INTERVAL + packetDelay
        //   FlowGen中InjectFlushTicker的额外Delay：_TIME_SLOT_UNIT
        //   FlowGen中输出队列Flush的Delay：flushInterval
        //   FlowGen中其它处理流程可能产生的Delay: 5s
        let second_quadruple_tolerable_delay = (yaml_config.packet_delay.as_secs()
            + 1
            + yaml_config.flow.flush_interval.as_secs()
            + COMMON_DELAY as u64)
            + yaml_config.second_flow_extra_delay.as_secs();
        let minute_quadruple_tolerable_delay = (60 + yaml_config.packet_delay.as_secs())
            + 1
            + yaml_config.flow.flush_interval.as_secs()
            + COMMON_DELAY as u64;

        let quadruple_generator = QuadrupleGeneratorThread::new(
            id,
            flow_receiver,
            second_sender,
            minute_sender,
            l4_log_sender,
            (yaml_config.flow.hash_slots << 3) as usize, // connection_lru_capacity
            metrics_type,
            second_quadruple_tolerable_delay,
            minute_quadruple_tolerable_delay,
            1 << 18, // possible_host_size
            config_handler.collector(),
            synchronizer.ntp_diff(),
            stats_collector.clone(),
        );

        let (l4_flow_aggr, flow_aggr_counter) = FlowAggrThread::new(
            id,                          // id
            l4_log_receiver,             // input
            l4_flow_aggr_sender.clone(), // output
            config_handler.collector(),
            synchronizer.ntp_diff(),
        );

        stats_collector.register_countable(
            "flow_aggr",
            Countable::Ref(Arc::downgrade(&flow_aggr_counter) as Weak<dyn RefCountable>),
            Default::default(),
        );

        let (mut second_collector, mut minute_collector) = (None, None);
        if metrics_type.contains(MetricsType::SECOND) {
            second_collector = Some(Collector::new(
                id as u32,
                second_receiver,
                metrics_sender.clone(),
                MetricsType::SECOND,
                second_quadruple_tolerable_delay as u32 + COMMON_DELAY, // qg processing is delayed and requires the collector component to increase the window size
                &stats_collector,
                config_handler.collector(),
                synchronizer.ntp_diff(),
            ));
        }
        if metrics_type.contains(MetricsType::MINUTE) {
            minute_collector = Some(Collector::new(
                id as u32,
                minute_receiver,
                metrics_sender,
                MetricsType::MINUTE,
                minute_quadruple_tolerable_delay as u32 + COMMON_DELAY, // qg processing is delayed and requires the collector component to increase the window size
                &stats_collector,
                config_handler.collector(),
                synchronizer.ntp_diff(),
            ));
        }

        CollectorThread::new(
            quadruple_generator,
            Some(l4_flow_aggr),
            second_collector,
            minute_collector,
        )
    }

    fn stop(&mut self) {
        if !self.running.swap(false, Ordering::Relaxed) {
            return;
        }

        for d in self.dispatchers.iter_mut() {
            d.stop();
        }
        self.platform_synchronizer.stop();

        #[cfg(target_os = "linux")]
        self.api_watcher.stop();

        for q in self.collectors.iter_mut() {
            q.stop();
        }

        for p in self.log_parsers.iter() {
            p.stop();
        }

        self.l4_flow_uniform_sender.stop();
        self.metrics_uniform_sender.stop();
        self.l7_flow_uniform_sender.stop();

        self.libvirt_xml_extractor.stop();
        self.debugger.stop();
        #[cfg(target_os = "linux")]
        if let Some(ebpf_collector) = self.ebpf_collector.as_mut() {
            ebpf_collector.stop();
        }
        #[cfg(target_os = "linux")]
        match self.cgroups_controller.stop() {
            Ok(_) => {
                info!("stopped cgroups_controller");
            }
            Err(e) => {
                warn!("stop cgroups_controller failed: {}", e);
            }
        }

        self.external_metrics_server.stop();
        self.otel_uniform_sender.stop();
        self.compressed_otel_uniform_sender.stop();
        self.prometheus_uniform_sender.stop();
        self.telegraf_uniform_sender.stop();
        self.packet_sequence_uniform_sender.stop(); // Enterprise Edition Feature: packet-sequence
        self.domain_name_listener.stop();
        self.handler_builders.iter().for_each(|x| {
            x.lock().unwrap().iter_mut().for_each(|y| {
                y.stop();
            })
        });
        self.pcap_manager.stop();
        info!("Stopped components.")
    }
}
