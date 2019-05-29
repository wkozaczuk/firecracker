// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_arch = "x86_64")]
extern crate backtrace;
#[macro_use(crate_version, crate_authors)]
extern crate clap;

extern crate api_server;
#[macro_use]
extern crate logger;
extern crate seccomp;
extern crate vmm;

#[cfg(target_arch = "x86_64")]
use backtrace::Backtrace;
use clap::{App, Arg};

use std::panic;
use std::process;
use std::sync::{Arc, RwLock};
use std::collections::VecDeque;
use std::path::PathBuf;

use logger::{Metric, LOGGER, METRICS};
use vmm::signal_handler::register_signal_handlers;
use vmm::KernelConfiguration;
use vmm::vmm_config::instance_info::{InstanceInfo, InstanceState};
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::machine_config::VmConfig;
use vmm::vmm_config::net::NetworkInterfaceConfig;

const DEFAULT_INSTANCE_ID: &str = "anonymous-instance";

fn main() {
    LOGGER
        .preinit(Some(DEFAULT_INSTANCE_ID.to_string()))
        .expect("Failed to register logger");

    error!("Main START");

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
            Arg::with_name("kernel-image-path")
                .long("kernel-image-path")
                .help("Path to kernel image path")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("kernel-cmdline")
                .long("kernel-cmdline")
                .help("Kernel command line arguments")
                .takes_value(true),
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
            Arg::with_name("block-device-path")
                .long("block-device-path")
                .help("Block device raw path")
                .takes_value(true),
        )
        .get_matches();

    // It's safe to unwrap here because clap's been provided with a default value,
    // and allowed values are guaranteed to parse to u32.
    let seccomp_level = cmd_arguments
        .value_of("seccomp-level")
        .unwrap()
        .parse::<u32>()
        .unwrap();

    let shared_info = Arc::new(RwLock::new(InstanceInfo {
        state: InstanceState::Uninitialized,
        id: String::from(DEFAULT_INSTANCE_ID),
        vmm_version: crate_version!().to_string(),
    }));

    let kernel_config = KernelConfiguration {
        kernel_image_path: String::from(cmd_arguments.value_of("kernel-image-path").unwrap()),
        kernel_cmdline: cmd_arguments.value_of("kernel-cmdline").map(|s| String::from(s)),
    };

    let vm_config = VmConfig {
        vcpu_count: Some(1),
        mem_size_mib: Some(64),
        ht_enabled: Some(false),
        cpu_template: None,
    };

    let root_block_device = BlockDeviceConfig {
        drive_id: String::from("rootfs"),
        path_on_host: PathBuf::from(cmd_arguments.value_of("block-device-path").unwrap()),
        is_root_device: false,
        partuuid: None,
        is_read_only: false,
        rate_limiter: None,
    };
    let mut block_devices = VecDeque::<BlockDeviceConfig>::new();
    block_devices.push_front(root_block_device);

    vmm::start_vmm_without_api(
        shared_info,
        seccomp_level,
        kernel_config,
        Some(vm_config),
        block_devices,
        VecDeque::<NetworkInterfaceConfig>::new()
    );
}