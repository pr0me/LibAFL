#[cfg(feature = "std")]
use serde::de::DeserializeOwned;

#[cfg(feature = "std")]
use crate::{
    bolts::os::startable_self,
    bolts::shmem::ShMemProvider,
    events::{LlmpRestartingEventManager, ManagerKind, RestartingMgr},
    inputs::Input,
    state::IfInteresting,
    stats::Stats,
    Error,
};

#[cfg(feature = "std")]
use std::net::SocketAddr;
#[cfg(all(windows, feature = "std"))]
use std::process::Stdio;

#[cfg(feature = "std")]
use core_affinity::CoreId;

#[cfg(feature = "std")]
use typed_builder::TypedBuilder;

/// Provides a Launcher, which can be used to launch a fuzzing run on a specified list of cores
#[cfg(feature = "std")]
#[derive(TypedBuilder)]
#[allow(clippy::type_complexity)]
pub struct Launcher<'a, I, S, SP, ST>
where
    I: Input,
    ST: Stats,
    SP: ShMemProvider + 'static,
    S: DeserializeOwned + IfInteresting<I>,
{
    /// The ShmemProvider to use
    shmem_provider: SP,
    /// The stats instance to use
    stats: ST,
    /// A closure or function which generates stats instances for newly spawned clients
    client_init_stats: &'a mut dyn FnMut() -> Result<ST, Error>,
    /// The 'main' function to run for each client forked. This probably shouldn't return
    run_client:
        &'a mut dyn FnMut(Option<S>, LlmpRestartingEventManager<I, S, SP, ST>) -> Result<(), Error>,
    /// The broker port to use
    #[builder(default = 1337_u16)]
    broker_port: u16,
    /// The list of cores to run on
    cores: &'a [usize],
    /// A file name to write all client output to
    #[builder(default = None)]
    stdout_file: Option<&'a str>,
    /// The `ip:port` address of another broker to connect our new broker to for multi-machine
    /// clusters.
    #[builder(default = None)]
    remote_broker_addr: Option<SocketAddr>,
}

#[cfg(feature = "std")]
impl<'a, I, S, SP, ST> Launcher<'a, I, S, SP, ST>
where
    I: Input,
    ST: Stats + Clone,
    SP: ShMemProvider + 'static,
    S: DeserializeOwned + IfInteresting<I>,
{
    /// Launch the broker and the clients and fuzz
    #[cfg(all(unix, feature = "std"))]
    #[allow(clippy::similar_names)]
    pub fn launch(&mut self) -> Result<(), Error> {
        let core_ids = core_affinity::get_core_ids().unwrap();
        let num_cores = core_ids.len();
        let mut handles = vec![];

        println!("spawning on cores: {:?}", self.cores);
        let file = self
            .stdout_file
            .map(|filename| File::create(filename).unwrap());

        //spawn clients
        for (id, bind_to) in core_ids.iter().enumerate().take(num_cores) {
            if self.cores.iter().any(|&x| x == id) {
                self.shmem_provider.pre_fork()?;
                match unsafe { fork() }? {
                    ForkResult::Parent(child) => {
                        self.shmem_provider.post_fork(false)?;
                        handles.push(child.pid);
                        #[cfg(feature = "std")]
                        println!("child spawned and bound to core {}", id);
                    }
                    ForkResult::Child => {
                        self.shmem_provider.post_fork(true)?;

                        #[cfg(feature = "std")]
                        std::thread::sleep(std::time::Duration::from_secs((id + 1) as u64));

                        #[cfg(feature = "std")]
                        if file.is_some() {
                            dup2(file.as_ref().unwrap().as_raw_fd(), libc::STDOUT_FILENO)?;
                            dup2(file.as_ref().unwrap().as_raw_fd(), libc::STDERR_FILENO)?;
                        }
                        //fuzzer client. keeps retrying the connection to broker till the broker starts
                        let stats = (self.client_init_stats)()?;
                        let (state, mgr) = RestartingMgr::builder()
                            .shmem_provider(self.shmem_provider.clone())
                            .stats(stats)
                            .broker_port(self.broker_port)
                            .kind(ManagerKind::Client {
                                cpu_core: Some(*bind_to),
                            })
                            .build()
                            .launch()?;

                        (self.run_client)(state, mgr)?;
                        break;
                    }
                };
            }
        }
        #[cfg(feature = "std")]
        println!("I am broker!!.");

        RestartingMgr::<I, S, SP, ST>::builder()
            .shmem_provider(self.shmem_provider.clone())
            .stats(self.stats.clone())
            .broker_port(self.broker_port)
            .kind(ManagerKind::Broker)
            .remote_broker_addr(self.remote_broker_addr)
            .build()
            .launch()?;

        //broker exited. kill all clients.
        for handle in &handles {
            unsafe {
                libc::kill(*handle, libc::SIGINT);
            }
        }

        Ok(())
    }

    /// Launch the broker and the clients and fuzz
    #[cfg(all(windows, feature = "std"))]
    #[allow(unused_mut)]
    pub fn launch(&mut self) -> Result<(), Error> {
        let is_client = std::env::var(_AFL_LAUNCHER_CLIENT);

        let mut handles = match is_client {
            Ok(core_conf) => {
                //todo: silence stdout and stderr for clients

                // the actual client. do the fuzzing
                let stats = (self.client_init_stats)()?;
                let (state, mgr) = RestartingMgr::<I, S, SP, ST>::builder()
                    .shmem_provider(self.shmem_provider.clone())
                    .stats(stats)
                    .broker_port(self.broker_port)
                    .kind(ManagerKind::Client {
                        cpu_core: Some(CoreId {
                            id: core_conf.parse()?,
                        }),
                    })
                    .build()
                    .launch()?;

                (self.run_client)(state, mgr)?;

                unreachable!("Fuzzer client code should never get here!");
            }
            Err(std::env::VarError::NotPresent) => {
                // I am a broker
                // before going to the broker loop, spawn n clients

                if self.stdout_file.is_some() {
                    println!("Child process file stdio is not supported on Windows yet. Dumping to stdout instead...");
                }

                let core_ids = core_affinity::get_core_ids().unwrap();
                let num_cores = core_ids.len();
                let mut handles = vec![];

                println!("spawning on cores: {:?}", self.cores);

                //spawn clients
                for (id, _) in core_ids.iter().enumerate().take(num_cores) {
                    if self.cores.iter().any(|&x| x == id) {
                        for id in 0..num_cores {
                            let stdio = if self.stdout_file.is_some() {
                                Stdio::inherit()
                            } else {
                                Stdio::null()
                            };

                            if self.cores.iter().any(|&x| x == id) {
                                std::env::set_var(_AFL_LAUNCHER_CLIENT, id.to_string());
                                let child = startable_self()?.stdout(stdio).spawn()?;
                                handles.push(child);
                            }
                        }
                    }
                }

                handles
            }
            Err(_) => panic!("Env variables are broken, received non-unicode!"),
        };

        #[cfg(feature = "std")]
        println!("I am broker!!.");

        RestartingMgr::<I, S, SP, ST>::builder()
            .shmem_provider(self.shmem_provider.clone())
            .stats(self.stats.clone())
            .broker_port(self.broker_port)
            .kind(ManagerKind::Broker)
            .remote_broker_addr(self.remote_broker_addr)
            .build()
            .launch()?;

        //broker exited. kill all clients.
        for handle in &mut handles {
            handle.kill()?;
        }

        Ok(())
    }
}

const _AFL_LAUNCHER_CLIENT: &str = &"AFL_LAUNCHER_CLIENT";