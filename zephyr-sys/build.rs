// Copyright (c) 2024 Linaro LTD
// SPDX-License-Identifier: Apache-2.0

// Pre-build code for zephyr module.

// This module makes the values from the generated .config available as conditional compilation.
// Note that this only applies to the zephyr module, and the user's application will not be able to
// see these definitions.  To make that work, this will need to be moved into a support crate which
// can be invoked by the user's build.rs.

// This builds a program that is run on the compilation host before the code is compiled.  It can
// output configuration settings that affect the compilation.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bindgen::Builder;

fn main() -> anyhow::Result<()> {
    // Point bindgen/clang-sys at the libclang shipped with the Zephyr SDK (if any) before
    // triggering any clang-sys initialization below.  This avoids relying on the host
    // environment to have libclang installed and discoverable on its own.
    configure_libclang_path();

    // Determine which version of Clang we linked with.
    let version = bindgen::clang_version();
    println!("Clang version: {:?}", version);

    // Pass in the target used to build the native code.
    let target = env::var("TARGET")?;

    // And get the root of the zephyr tree.
    let zephyr_base = env::var("ZEPHYR_BASE")?;

    // Rustc uses some complex target tuples for the riscv targets, whereas clang uses other
    // options.  Fortunately, these variants shouldn't affect the structures generated, so just
    // turn this into a generic target.
    let target = if target.starts_with("riscv32") {
        "riscv32-unknown-none-elf".to_string()
    } else {
        target
    };

    // Likewise, do the same with RISCV-64.
    let target = if target.starts_with("riscv64") {
        "riscv64-unknown-none-elf".to_string()
    } else {
        target
    };

    let target_arg = format!("--target={}", target);

    // println!("includes: {:?}", env::var("INCLUDE_DIRS"));
    // println!("defines: {:?}", env::var("INCLUDE_DEFINES"));

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("missing output directory"));
    let wrapper_path = PathBuf::from(env::var("WRAPPER_FILE").expect("missing wrapper file"));

    // Bindgen everything.
    let bindings = Builder::default()
        .clang_arg("-DRUST_BINDGEN")
        .clang_arg(format!("-I{}/lib/libc/minimal/include", zephyr_base))
        .clang_arg(&target_arg)
        .header(
            Path::new("wrapper.h")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .use_core();

    let bindings = define_args(bindings, "-I", "INCLUDE_DIRS");
    let bindings = define_args(bindings, "-D", "INCLUDE_DEFINES");

    let bindings = bindings
        .wrap_static_fns(true)
        .wrap_static_fns_path(wrapper_path);

    let bindings = bindings.derive_copy(false).derive_default(true);

    let bindings = bindings
        // Deprecated
        .blocklist_function("sys_clock_timeout_end_calc")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    let dotconfig = env::var("DOTCONFIG").expect("missing DOTCONFIG path");
    let options = zephyr_build::extract_kconfig_bool_options(&dotconfig)
        .expect("failed to extract kconfig boolean options");

    let bindings = bindings
        // Kernel
        .allowlist_item("E.*")
        .allowlist_item("K_.*")
        .allowlist_item("LOG_.*")
        .allowlist_item("Z_.*")
        .allowlist_item("ZR_.*")
        .allowlist_item("LOG_LEVEL_.*")
        .allowlist_item("k_poll_modes")
        // Each DT node has a device entry that is a static.
        .allowlist_item("__device_dts_ord.*")
        .allowlist_function("k_.*")
        .allowlist_function("z_log.*")
        .allowlist_function("sys_.*")
        .allowlist_function("zr_.*")
        .allowlist_function("device_.*")
        .allowlist_function("SEGGER.*")
        // Bluetooth
        .allowlist_item_if("CONFIG_BT_.*", || options.contains("CONFIG_BT"))
        .allowlist_function_if("bt_.*", || options.contains("CONFIG_BT"))
        // GPIO
        .allowlist_item_if("CONFIG_GPIO_.*", || options.contains("CONFIG_GPIO"))
        .allowlist_item_if("GPIO_.*", || options.contains("CONFIG_GPIO"))
        .allowlist_function_if("gpio_.*", || options.contains("CONFIG_GPIO"))
        // Flash
        .allowlist_item_if("FLASH_.*", || options.contains("CONFIG_FLASH"))
        .allowlist_function_if("flash_.*", || options.contains("CONFIG_FLASH"))
        // UART
        .allowlist_item_if("CONFIG_UART_.*", || options.contains("CONFIG_SERIAL"))
        .allowlist_function_if("uart_.*", || options.contains("CONFIG_SERIAL"))
        // Generate
        .generate()
        .expect("Unable to generate bindings");

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    Ok(())
}

/// Detect and configure a libclang usable by bindgen (via clang-sys).
///
/// If the user has already set `LIBCLANG_PATH`, that choice is respected and left untouched.
/// Otherwise, if the Zephyr SDK is available (SDK 1.0+ ships an LLVM toolchain including
/// libclang), probe its `llvm` directory for a usable libclang and point `LIBCLANG_PATH` at it.
/// This makes bindgen work out-of-the-box for anyone using Zephyr SDK 1.0+, without requiring a
/// separately installed libclang on the host.
fn configure_libclang_path() {
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=ZEPHYR_SDK_INSTALL_DIR");

    // Respect an explicit user override.
    if env::var_os("LIBCLANG_PATH").is_some() {
        return;
    }

    let Some(sdk_dir) = env::var_os("ZEPHYR_SDK_INSTALL_DIR") else {
        return;
    };
    let sdk_dir = PathBuf::from(sdk_dir);

    // Directories within the SDK's LLVM toolchain that could hold a libclang shared library,
    // depending on host OS (Linux/macOS use `llvm/lib`, Windows uses `llvm/bin` for DLLs).
    let candidate_dirs = [sdk_dir.join("llvm").join("lib"), sdk_dir.join("llvm").join("bin")];

    for dir in candidate_dirs {
        if find_libclang(&dir) {
            println!(
                "cargo:warning=Using libclang from Zephyr SDK at {}",
                dir.display()
            );
            env::set_var("LIBCLANG_PATH", &dir);
            return;
        }
    }
}

/// Returns true if `dir` contains a file that looks like a libclang shared library.
fn find_libclang(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    entries.filter_map(|entry| entry.ok()).any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("libclang.") || name.starts_with("libclang-")
    })
}

trait BuilderExt {
    type B;

    fn allowlist_function_if<P>(self, pattern: &str, pred: P) -> Self::B
    where
        P: FnOnce() -> bool;

    fn allowlist_item_if<P>(self, pattern: &str, pred: P) -> Self::B
    where
        P: FnOnce() -> bool;
}

impl BuilderExt for Builder {
    type B = Builder;

    fn allowlist_function_if<P>(self, pattern: &str, pred: P) -> Self::B
    where
        P: FnOnce() -> bool,
    {
        if pred() {
            return self.allowlist_function(pattern);
        }
        self
    }

    fn allowlist_item_if<P>(self, pattern: &str, pred: P) -> Self::B
    where
        P: FnOnce() -> bool,
    {
        if pred() {
            return self.allowlist_item(pattern);
        }
        self
    }
}

fn define_args(bindings: Builder, prefix: &str, var_name: &str) -> Builder {
    let text = env::var(var_name).expect("missing environment variable");
    let mut bindings = bindings;
    // Split on either spaces or semicolons, to allow some flexibility in what cmake might generate
    // for us.
    for entry in text.split(&[' ', ';']) {
        if entry.is_empty() {
            continue;
        }
        println!("Entry: {}{}", prefix, entry);
        let arg = format!("{}{}", prefix, entry);
        bindings = bindings.clang_arg(arg);
    }
    bindings
}
