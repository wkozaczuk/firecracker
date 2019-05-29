// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_arch = "x86_64")]
extern crate backtrace;
#[macro_use(crate_version, crate_authors)]
extern crate clap;

extern crate api_server;
extern crate fc_util;
extern crate jailer;
#[macro_use]
extern crate logger;
extern crate mmds;
extern crate seccomp;
extern crate vmm;

#[cfg(target_arch = "x86_64")]
use backtrace::Backtrace;
use clap::{App, Arg};

use std::io::ErrorKind;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};

use api_server::{ApiServer, Error};
use fc_util::validators::validate_instance_id;
use logger::{Metric, LOGGER, METRICS};
use mmds::MMDS;
use vmm::signal_handler::register_signal_handlers;
use vmm::vmm_config::instance_info::{InstanceInfo, InstanceState};

const DEFAULT_API_SOCK_PATH: &str = "/tmp/firecracker.socket";
const DEFAULT_INSTANCE_ID: &str = "anonymous-instance";

fn main() {
    LOGGER
        .preinit(Some(DEFAULT_INSTANCE_ID.to_string()))
        .expect("Failed to register logger");

    if let Err(e) = register_signal_handlers() {
        error!("Failed to register signal handlers: {}", e);
        process::exit(i32::from(vmm::FC_EXIT_CODE_GENERIC_ERROR));
    }
    // Start firecracker by setting up a panic hook, which will be called before
    // terminating as we're building with panic = "abort".
    // It's worth noting that the abort is caused by sending a SIG_ABORT signal to the process.
    panic::set_hook(Box::new(move |info| {
        // We're currently using the closure parameter, which is a &PanicInfo, for printing the
        // origin of the panic, including the payload passed to panic! and the source code location
        // from which the panic originated.
        error!("Firecracker {}", info);
        METRICS.vmm.panic_count.inc();
        #[cfg(target_arch = "x86_64")]
        {
            let bt = Backtrace::new();
            error!("{:?}", bt);
        }

        // Log the metrics before aborting.
        if let Err(e) = LOGGER.log_metrics() {
            error!("Failed to log metrics while panicking: {}", e);
        }
    }));

    let cmd_arguments = App::new("firecracker")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a microvm.")
        .arg(
            Arg::with_name("api_sock")
                .long("api-sock")
                .help("Path to unix domain socket used by the API")
                .default_value(DEFAULT_API_SOCK_PATH)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("id")
                .long("id")
                .help("MicroVM unique identifier")
                .default_value(DEFAULT_API_SOCK_PATH)
                .takes_value(true)
                .default_value(DEFAULT_INSTANCE_ID)
                .validator(|s: String| -> Result<(), String> {
                    validate_instance_id(&s).map_err(|e| format!("{}", e))
                }),
        )
        .arg(
            Arg::with_name("seccomp-level")
                .long("seccomp-level")
                .help(
                    "Level of seccomp filtering.\n
                            - Level 0: No filtering.\n
                            - Level 1: Seccomp filtering by syscall number.\n
                            - Level 2: Seccomp filtering by syscall number and argument values.\n
                        ",
                )
                .takes_value(true)
                .default_value("2")
                .possible_values(&["0", "1", "2"]),
        )
        .arg(
            Arg::with_name("start-time-us")
                .long("start-time-us")
                .takes_value(true)
                .hidden(true),
        )
        .arg(
            Arg::with_name("start-time-cpu-us")
                .long("start-time-cpu-us")
                .takes_value(true)
                .hidden(true),
        )
        .get_matches();

    let bind_path = cmd_arguments
        .value_of("api_sock")
        .map(PathBuf::from)
        .expect("Missing argument: api_sock");

    // It's safe to unwrap here because clap's been provided with a default value
    let instance_id = cmd_arguments.value_of("id").unwrap().to_string();

    // We disable seccomp filtering when testing, because when running the test_gnutests
    // integration test from test_unittests.py, an invalid syscall is issued, and we crash
    // otherwise.
    #[cfg(test)]
    let seccomp_level = seccomp::SECCOMP_LEVEL_NONE;
    #[cfg(not(test))]
    // It's safe to unwrap here because clap's been provided with a default value,
    // and allowed values are guaranteed to parse to u32.
    let seccomp_level = cmd_arguments
        .value_of("seccomp-level")
        .unwrap()
        .parse::<u32>()
        .unwrap();

    let start_time_us = cmd_arguments.value_of("start-time-us").map(|s| {
        s.parse::<u64>()
            .expect("'start-time-us' parameter expected to be of 'u64' type.")
    });

    let start_time_cpu_us = cmd_arguments.value_of("start-time-cpu-us").map(|s| {
        s.parse::<u64>()
            .expect("'start-time-cpu_us' parameter expected to be of 'u64' type.")
    });

    let shared_info = Arc::new(RwLock::new(InstanceInfo {
        state: InstanceState::Uninitialized,
        id: instance_id,
        vmm_version: crate_version!().to_string(),
    }));
    let mmds_info = MMDS.clone();
    let (to_vmm, from_api) = channel();
    let server =
        ApiServer::new(mmds_info, shared_info.clone(), to_vmm).expect("Cannot create API server");

    let api_event_fd = server
        .get_event_fd_clone()
        .expect("Cannot clone API eventFD.");

    let _vmm_thread_handle =
        vmm::start_vmm_thread(shared_info, api_event_fd, from_api, seccomp_level);

    match server.bind_and_run(bind_path, start_time_us, start_time_cpu_us, seccomp_level) {
        Ok(_) => (),
        Err(Error::Io(inner)) => match inner.kind() {
            ErrorKind::AddrInUse => panic!("Failed to open the API socket: {:?}", Error::Io(inner)),
            _ => panic!(
                "Failed to communicate with the API socket: {:?}",
                Error::Io(inner)
            ),
        },
        Err(eventfd_err @ Error::Eventfd(_)) => {
            panic!("Failed to open the API socket: {:?}", eventfd_err)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate tempfile;

    use self::tempfile::NamedTempFile;
    use super::*;

    use logger::AppInfo;
    use std::fs::File;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::path::Path;
    use std::time::Duration;
    use std::{fs, thread};

    /// Look through the log for lines that match expectations.
    /// expectations is a list of tuples of words we're looking for.
    /// A tuple matches a line if all the words in the tuple can be found on that line.
    /// For this test to pass, every tuple must match at least one line.
    fn validate_backtrace(
        log_path: &str,
        expectations: &[(&'static str, &'static str, &'static str)],
    ) -> bool {
        let f = File::open(log_path).unwrap();
        let reader = BufReader::new(f);
        let mut expectation_iter = expectations.iter();
        let mut expected_words = expectation_iter.next().unwrap();

        for ln_res in reader.lines() {
            let line = ln_res.unwrap();
            if !(line.contains(expected_words.0)
                && line.contains(expected_words.1)
                && line.contains(expected_words.2))
            {
                continue;
            }
            if let Some(w) = expectation_iter.next() {
                expected_words = w;
                continue;
            }
            return true;
        }
        false
    }

    #[test]
    #[allow(clippy::unit_cmp)]
    fn test_main() {
        const FIRECRACKER_INIT_TIMEOUT_MILLIS: u64 = 100;

        // There is no reason to run this test if the default socket path exists.
        assert!(!Path::new(DEFAULT_API_SOCK_PATH).exists());

        let log_file_temp =
            NamedTempFile::new().expect("Failed to create temporary output logging file.");
        let metrics_file_temp =
            NamedTempFile::new().expect("Failed to create temporary metrics logging file.");
        let log_file = String::from(log_file_temp.path().to_path_buf().to_str().unwrap());

        // Start Firecracker in a separate thread
        thread::spawn(|| {
            main();
        });

        // Wait around for a bit, so Firecracker has time to initialize and create the
        // API socket.
        thread::sleep(Duration::from_millis(FIRECRACKER_INIT_TIMEOUT_MILLIS));

        // If Firecracker hasn't finished initializing yet, something is really wrong!
        assert!(Path::new(DEFAULT_API_SOCK_PATH).exists());

        // Initialize the logger
        LOGGER
            .init(
                &AppInfo::new("Firecracker", "1.0"),
                "TEST-ID",
                log_file_temp.path().to_str().unwrap().to_string(),
                metrics_file_temp.path().to_str().unwrap().to_string(),
                &[],
            )
            .expect("Could not initialize logger.");

        // Cause some controlled panic and see if a backtrace shows up in the log,
        // as it's supposed to.
        let _ = panic::catch_unwind(|| {
            panic!("Oh, noes!");
        });
        // Look for the expected backtrace inside the log
        assert!(
            validate_backtrace(
                log_file.as_str(),
                &[
                    // Lines containing these words should have appeared in the log, in this order
                    ("ERROR", "main.rs", "Firecracker panicked at"),
                    ("ERROR", "main.rs", "stack backtrace:"),
                    ("0:", "0x", "firecracker::main::"),
                ],
            ) || println!("Could not validate backtrace!\n {:?}", Backtrace::new()) != ()
        );

        // Clean up
        fs::remove_file(DEFAULT_API_SOCK_PATH).expect("failure in removing socket file");
    }
}
