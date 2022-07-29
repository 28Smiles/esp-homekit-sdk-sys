use std::convert::TryFrom;
use std::ffi::OsStr;
use std::{env, path::PathBuf};

use anyhow::*;

use embuild::bindgen;
use embuild::build;
use embuild::cargo;
use embuild::kconfig;
use embuild::pio;
use embuild::pio::project;

use walkdir::WalkDir;

fn main() -> Result<()> {
    let pio = pio::Pio::install_default()?;

    let resolution = pio::Resolver::new(pio.clone())
        .params(pio::ResolutionParams {
            platform: Some("espressif32".into()),
            frameworks: vec!["espidf".into()],
            target: Some(env::var("TARGET")?),
            ..Default::default()
        })
        .resolve(true)?;

    let mut builder =
        project::Builder::new(PathBuf::from(env::var("OUT_DIR")?).join("esp-homekit-sdk"));

    dotenv::var("ESP_IDF_SYS_PIO_CONF_HOMEKIT_0")?;

    builder
        .enable_scons_dump()
        .enable_c_entry_points()
        .options(build::env_options_iter("ESP_IDF_SYS_PIO_CONF_HOMEKIT")?)
        .files(build::tracked_env_globs_iter("ESP_IDF_SYS_GLOB")?);

    let project_path = builder.generate(&resolution)?;

    pio.exec_with_args(&[
        OsStr::new("lib"),
        OsStr::new("--global"),
        OsStr::new("install"),
    ])?;

    pio.build(&project_path, env::var("PROFILE")? == "release")?;

    let pio_scons_vars = project::SconsVariables::from_dump(&project_path)?;

    build::LinkArgsBuilder::try_from(&pio_scons_vars)?
        .build()?
        .propagate();

    build::CInclArgs::try_from(&pio_scons_vars)?.propagate();

    let kconfig_str_allow = regex::Regex::new(r"IDF_TARGET")?;
    let cfg_args = build::CfgArgs {
        args: kconfig::try_from_config_file(
            pio_scons_vars
                .project_dir
                .join(if pio_scons_vars.release_build {
                    "sdkconfig.release"
                } else {
                    "sdkconfig.debug"
                })
                .as_path()
        )?
            .filter(|(key, value)| {
                matches!(value, kconfig::Value::Tristate(kconfig::Tristate::True))
                    || kconfig_str_allow.is_match(key)
            })
            .filter_map(|(key, value)| value.to_rustc_cfg("esp_idf", key))
            .collect::<Vec<String>>()
    };

    cfg_args.propagate();
    cfg_args.output();

    let header = PathBuf::from("src").join("include").join("bindings.h");

    cargo::track_file(&header);

    let d = PathBuf::from(env::var("OUT_DIR")?)
        .join("esp-homekit-sdk/.pio/libdeps/debug/esp-homekit-sdk/components")
        .display()
        .to_string();

    let mut args = vec![
        format!(
            "-I{}",
            PathBuf::from(env::var("OUT_DIR")?)
                .join("esp-homekit-sdk/.pio/libdeps/debug/esp-homekit-sdk/components/common/app_wifi")
                .display()
                .to_string()
        ),
        format!(
            "-I{}",
            PathBuf::from(env::var("OUT_DIR")?)
                .join("esp-homekit-sdk/.pio/libdeps/debug/esp-homekit-sdk/components/common/app_hap_setup_payload")
                .display()
                .to_string(),
        ),
        format!(
            "-I{}",
            PathBuf::from(env::var("OUT_DIR")?)
                .join("esp-homekit-sdk/.pio/libdeps/debug/esp-homekit-sdk/components/common/qrcode/include")
                .display()
                .to_string(),
        ),
    ];

    for entry in WalkDir::new(d).into_iter().filter_map(|e| e.ok()) {
        if entry.path().ends_with("include") {
            args.push(format!("-I{}", entry.path().display().to_string()));
        }
        if entry.path().ends_with("ld") {
            args.push(format!("-L{}", entry.path().display().to_string()));
        }
    }

    bindgen::run(
        bindgen::Factory::from_scons_vars(&pio_scons_vars)?
            .builder()?
            .ctypes_prefix("c_types")
            .header(header.to_string_lossy())
            .blocklist_function("strtold")
            .blocklist_function("_strtold_r")
            .clang_args(args)
    )?;

    Ok(())
}
