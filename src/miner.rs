use crate::config::Cfg;
use crate::cpu_hasher::SimdExtension;
#[cfg(feature = "opencl")]
use crate::ocl::GpuConfig;
use crate::poc_hashing;
use crate::request::RequestHandler;
use crate::scheduler::create_scheduler_thread;
use crate::scheduler::RoundInfo;
use crossbeam_channel::unbounded;
use futures::sync::mpsc;
use std::cmp::{max, min};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::u64;
use tokio::prelude::*;
use tokio::runtime::TaskExecutor;
use tokio::timer::Interval;
use url::Url;

const GENESIS_BASE_TARGET: u64 = 4_398_046_511_104;

pub struct Miner {
    executor: TaskExecutor,
    request_handler: RequestHandler,
    cpu_threads: usize,
    cpu_worker_task_size: u64,
    simd_extensions: SimdExtension,
    numeric_id: u64,
    start_nonce: u64,
    target_deadline: u64,
    blocktime: u64,
    gpus: Vec<GpuConfig>,
}

pub struct State {
    generation_signature: String,
    base_target: u64,
    height: u64,
    server_target_deadline: u64,
    first: bool,
    outage: bool,
    best_deadline: u64,
}

#[derive(Clone)]
pub struct NonceData {
    pub numeric_id: u64,
    pub nonce: u64,
    pub height: u64,
    pub deadline: u64,
    pub deadline_adjusted: u64,
    pub capacity: u64
}

impl Miner {
    pub fn new(
        cfg: Cfg,
        simd_extensions: SimdExtension,
        cpu_threads: usize,
        executor: TaskExecutor,
    ) -> Miner {
        let base_url = Url::parse(&cfg.url).expect("invalid mining server url");
        info!("server: {}", base_url);
        let request_handler = RequestHandler::new(
            base_url,
            cfg.secret_phrase,
            cfg.timeout,
            min(cfg.timeout, max(500, cfg.get_mining_info_interval) - 200),
            cfg.send_proxy_details,
            cfg.additional_headers,
        );

        Miner {
            executor,
            request_handler,
            cpu_threads,
            cpu_worker_task_size: cfg.cpu_worker_task_size,
            simd_extensions,
            numeric_id: cfg.numeric_id,
            start_nonce: cfg.start_nonce,
            target_deadline: cfg.target_deadline,
            blocktime: cfg.blocktime,
            gpus: cfg.gpus,
        }
    }

    pub fn run(self) {
        // create channels
        let (tx_rounds, rx_rounds) = unbounded();
        let (tx_nonce_data, rx_nonce_data) = mpsc::unbounded();

        // create hasher thread
        thread::spawn(create_scheduler_thread(
            self.numeric_id,
            self.start_nonce,
            self.cpu_threads as u8,
            self.cpu_worker_task_size,
            self.simd_extensions.clone(),
            self.gpus,
            self.blocktime,
            rx_rounds.clone(),
            tx_nonce_data.clone(),
        ));

        let state = Arc::new(Mutex::new(State {
            generation_signature: "".to_owned(),
            height: 0,
            server_target_deadline: u64::MAX,
            base_target: 1,
            first: true,
            outage: false,
            best_deadline: u64::MAX,
        }));

        let request_handler = self.request_handler.clone();
        let inner_state = state.clone();
        let inner_tx_rounds = tx_rounds.clone();
        // run main mining loop on core
        self.executor.clone().spawn(
            Interval::new(Instant::now(), Duration::from_millis(1000))
                .for_each(move |_| {
                    let state = inner_state.clone();
                    let tx_rounds = inner_tx_rounds.clone();
                    request_handler.get_mining_info().then(move |mining_info| {
                        match mining_info {
                            Ok(mining_info) => {
                                let mut state = state.lock().unwrap();
                                if mining_info.generation_signature != state.generation_signature {
                                    state.generation_signature =
                                        mining_info.generation_signature.clone();
                                    state.height = mining_info.height;
                                    state.best_deadline = u64::MAX;
                                    state.base_target = mining_info.base_target;
                                    state.server_target_deadline = mining_info.target_deadline;

                                    let gensig = poc_hashing::decode_gensig(
                                        &mining_info.generation_signature,
                                    );

                                    let scoop =
                                        poc_hashing::calculate_scoop(mining_info.height, &gensig);
                                    info!(
                                        "{: <80}",
                                        format!(
                                            "new block: height={}, scoop={}, netdiff={}",
                                            mining_info.height,
                                            scoop,
                                            GENESIS_BASE_TARGET / 240 / mining_info.base_target,
                                        )
                                    );
                                    // communicate new round hasher
                                    tx_rounds
                                        .send(RoundInfo {
                                            gensig,
                                            base_target: state.base_target,
                                            scoop: scoop as u64,
                                            height: state.height,
                                        })
                                        .expect("main thread can't communicate with hasher thread");
                                }
                            }
                            _ => {
                                let mut state = state.lock().unwrap();
                                if state.first {
                                    error!(
                                        "{: <80}",
                                        "error getting mining info, please check server config"
                                    );
                                    state.first = false;
                                    state.outage = true;
                                } else {
                                    if !state.outage {
                                        error!(
                                            "{: <80}",
                                            "error getting mining info => connection outage..."
                                        );
                                    }
                                    state.outage = true;
                                }
                            }
                        }
                        future::ok(())
                    })
                })
                .map_err(|e| panic!("interval errored: err={:?}", e)),
        );

        let target_deadline = self.target_deadline;
        let request_handler = self.request_handler.clone();
        let state = state.clone();
        let inner_executor = self.executor.clone();
        self.executor.clone().spawn(
            rx_nonce_data
                .for_each(move |nonce_data| {
                    let mut state = state.lock().unwrap();
                    if state.height == nonce_data.height {
                        if state.best_deadline > nonce_data.deadline_adjusted
                            && nonce_data.deadline_adjusted < target_deadline
                        {
                            state.best_deadline = nonce_data.deadline_adjusted;
                            inner_executor.spawn(request_handler.submit_nonce(nonce_data));
                        }
                    }
                    Ok(())
                })
                .map_err(|e| panic!("interval errored: err={:?}", e)),
        );
    }
}
