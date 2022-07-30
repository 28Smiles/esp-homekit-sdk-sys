use std::convert::TryFrom;
use std::ffi::OsStr;
use std::{env, fs, path::PathBuf};
use std::fmt::Display;
use std::path::Path;

use anyhow::*;

use embuild::{bindgen, build};
use embuild::cargo;
use embuild::cargo::{IntoWarning, workspace_dir};
use embuild::kconfig;
use embuild::pio;
use embuild::pio::project;
use embuild::utils::{OsStrExt, PathExt};

use walkdir::WalkDir;

const ESP_IDF_TOOLS_INSTALL_DIR_VAR: &str = "ESP_IDF_TOOLS_INSTALL_DIR";
const ESP_IDF_SDKCONFIG_DEFAULTS_VAR: &str = "ESP_IDF_SDKCONFIG_DEFAULTS";
const ESP_IDF_SDKCONFIG_VAR: &str = "ESP_IDF_SDKCONFIG";
const MCU_VAR: &str = "MCU";
const SDKCONFIG_FILE: &str = "sdkconfig";
const SDKCONFIG_DEFAULTS_FILE: &str = "sdkconfig.defaults";
const TOOLS_WORKSPACE_INSTALL_DIR: &str = ".embuild";

fn list_specific_sdkconfigs(
    path: PathBuf,
    profile: &str,
    chip: &str,
) -> impl DoubleEndedIterator<Item = PathBuf> {
    path.file_name()
        .and_then(|filename| filename.try_to_str().into_warning())
        .map(|filename| {
            let profile_specific = format!("{}.{}", filename, profile);
            let chip_specific = format!("{}.{}", filename, chip);
            let profile_chip_specific = format!("{}.{}", &profile_specific, chip);

            [
                profile_chip_specific,
                chip_specific,
                profile_specific,
                filename.to_owned(),
            ]
        })
        .into_iter()
        .flatten()
        .filter_map(move |s| {
            let path = path.with_file_name(s);
            if path.is_file() {
                Some(path)
            } else {
                None
            }
        })
}

#[derive(Clone, Debug)]
enum InstallDir {
    Global,
    Workspace(PathBuf),
    Out(PathBuf),
    Custom(PathBuf),
    FromEnv,
}

impl InstallDir {
    /// Get the install directory from the [`ESP_IDF_TOOLS_INSTALL_DIR_VAR`] env variable.
    ///
    /// If this env variable is unset or empty uses `default_install_dir` instead.
    /// On success returns `(install_dir as InstallDir, is_default as bool)`.
    pub fn from_env_or(
        default_install_dir: &str,
        builder_name: &str,
    ) -> Result<(InstallDir, bool)> {
        let location = env::var_os(ESP_IDF_TOOLS_INSTALL_DIR_VAR);
        let (location, is_default) = match &location {
            None => (default_install_dir, true),
            Some(val) => {
                let val = val.try_to_str()?.trim();
                if val.is_empty() {
                    (default_install_dir, true)
                } else {
                    (val, false)
                }
            }
        };
        let install_dir = match location.to_lowercase().as_str() {
            "global" => Self::Global,
            "workspace" => Self::Workspace(
                workspace_dir().ok_or_else(|| anyhow!("No workspace"))?
                    .join(TOOLS_WORKSPACE_INSTALL_DIR)
                    .join(builder_name),
            ),
            "out" => Self::Out(cargo::out_dir().join(builder_name)),
            "fromenv" => Self::FromEnv,
            _ => Self::Custom({
                if let Some(suffix) = location.strip_prefix("custom:") {
                    Path::new(suffix).abspath_relative_to(&workspace_dir()
                        .ok_or_else(|| anyhow!("No workspace"))?)
                } else {
                    bail!(
                        "Invalid installation directory format. \
                         Should be one of `global`, `workspace`, `out`, `fromenv` or `custom:<dir>`."
                    );
                }
            }),
        };
        Ok((install_dir, is_default))
    }

    pub fn is_from_env(&self) -> bool {
        matches!(self, Self::FromEnv)
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Global | Self::FromEnv => None,
            Self::Workspace(ref path) => Some(path.as_ref()),
            Self::Out(ref path) => Some(path.as_ref()),
            Self::Custom(ref path) => Some(path.as_ref()),
        }
    }
}

impl Display for InstallDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global"),
            Self::Workspace(ref path) => write!(f, "workspace ({})", path.display()),
            Self::Out(ref path) => write!(f, "out ({})", path.display()),
            Self::Custom(ref path) => write!(f, "custom ({})", path.display()),
            Self::FromEnv => write!(f, "fromenv"),
        }
    }
}

fn main() -> Result<()> {
    let (pio_scons_vars, link_args) = if let Some(pio_scons_vars) =
    project::SconsVariables::from_piofirst()
    {
        println!("cargo:info=PIO->Cargo build detected: generating bindings only");

        (pio_scons_vars, None)
    } else {
        cargo::track_env_var(ESP_IDF_TOOLS_INSTALL_DIR_VAR);
        cargo::track_env_var(ESP_IDF_SDKCONFIG_VAR);
        cargo::track_env_var(ESP_IDF_SDKCONFIG_DEFAULTS_VAR);
        cargo::track_env_var(MCU_VAR);

        let out_dir = cargo::out_dir();
        let workspace_dir = workspace_dir().ok_or_else(|| anyhow!("No workspace"))?;
        let profile = env::var("PROFILE")
            .expect("No cargo `PROFILE` environment variable");

        // Get the install dir from the $ESP_IDF_TOOLS_INSTALL_DIR, if unset use
        // "workspace" and allow platformio from the environment.
        let (install_dir, allow_from_env) = InstallDir::from_env_or("workspace", "platformio")?;
        // Pio must come from the environment if $ESP_IDF_TOOLS_INSTALL_DIR == "fromenv".
        let require_from_env = install_dir.is_from_env();
        let maybe_from_env = require_from_env || allow_from_env;

        let install = |install_dir: &InstallDir| -> Result<pio::Pio> {
            let install_dir = install_dir.path().map(ToOwned::to_owned);

            if let Some(install_dir) = &install_dir {
                // Workaround an issue in embuild until it is fixed in the next version
                fs::create_dir_all(install_dir)?;
            }

            pio::Pio::install(install_dir, pio::LogLevel::Standard, false)
        };

        let pio = match (pio::Pio::try_from_env(), maybe_from_env) {
            (Some(pio), true) => {
                eprintln!(
                    "Using platformio from environment at '{}'",
                    pio.platformio_exe.display()
                );

                pio
            }
            (Some(_), false) => {
                cargo::print_warning(format_args!(
                    "Ignoring platformio in environment: ${ESP_IDF_TOOLS_INSTALL_DIR_VAR} != {}",
                    InstallDir::FromEnv
                ));
                install(&install_dir)?
            }
            (None, true) if require_from_env => {
                bail!(
                    "platformio not found in environment ($PATH) \
                       but required by ${ESP_IDF_TOOLS_INSTALL_DIR_VAR} == {install_dir}"
                );
            }
            (None, _) => install(&install_dir)?,
        };

        let resolution = pio::Resolver::new(pio.clone())
            .params(pio::ResolutionParams {
                platform: Some("espressif32".into()),
                frameworks: vec!["espidf".into()],
                mcu: env::var(MCU_VAR).ok(),
                target: Some(env::var("TARGET")?),
                ..Default::default()
            })
            .resolve(true)?;

        let mut builder = project::Builder::new(out_dir.join("esp-homekit-sdk"));

        // Resolve `ESP_IDF_SDKCONFIG` and `ESP_IDF_SDKCONFIG_DEFAULTS` to an absolute path
        // relative to the workspace directory if not empty.
        let sdkconfig = {
            let file = env::var_os(ESP_IDF_SDKCONFIG_VAR).unwrap_or_else(|| SDKCONFIG_FILE.into());
            let path = Path::new(&file).abspath_relative_to(&workspace_dir);
            let cfg = list_specific_sdkconfigs(path, &profile, &resolution.mcu).next();

            cfg.map(|path| {
                cargo::track_file(&path);

                (path, format!("sdkconfig.{}", profile).into())
            })
        };

        let sdkconfig_defaults_var = env::var_os(ESP_IDF_SDKCONFIG_DEFAULTS_VAR)
            .unwrap_or_else(|| SDKCONFIG_DEFAULTS_FILE.into());
        let sdkconfig_defaults = sdkconfig_defaults_var
            .try_to_str()?
            .split(';')
            .filter_map(|v| {
                if !v.is_empty() {
                    let path = Path::new(v).abspath_relative_to(&workspace_dir);
                    Some(
                        list_specific_sdkconfigs(path, &profile, &resolution.mcu)
                            // We need to reverse the order here so that the more
                            // specific defaults come last.
                            .rev(),
                    )
                } else {
                    None
                }
            })
            .flatten()
            .map(|path| {
                cargo::track_file(&path);
                let file_name = PathBuf::from(path.file_name().unwrap());
                (path, file_name)
            });

        dotenv::var("ESP_IDF_SYS_PIO_CONF_HOMEKIT_0")?;

        builder
            .enable_scons_dump()
            .enable_c_entry_points()
            .options(build::env_options_iter("ESP_IDF_SYS_PIO_CONF_HOMEKIT")?)
            .files(build::tracked_env_globs_iter("ESP_IDF_SYS_GLOB")?)
            .files(sdkconfig.into_iter())
            .files(sdkconfig_defaults);

        let project_path = builder.generate(&resolution)?;

        pio.exec_with_args(&[
            OsStr::new("lib"),
            OsStr::new("--global"),
            OsStr::new("install"),
        ])?;

        pio.build(&project_path, profile == "release")?;

        let pio_scons_vars = project::SconsVariables::from_dump(&project_path)?;

        let link_args = build::LinkArgsBuilder::try_from(&pio_scons_vars)?.build()?;

        (pio_scons_vars, Some(link_args))
    };

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

    let mcu = cfg_args.get("esp_idf_config_idf_target").ok_or_else(|| {
        anyhow!(
            "Failed to get IDF_TARGET from kconfig. cfgs:\n{:?}",
            cfg_args.args
        )
    })?;

    bindgen::run(
        bindgen::Factory::from_scons_vars(&pio_scons_vars)?
            .builder()?
            .ctypes_prefix("c_types")
            .header(header.to_string_lossy())
            .blocklist_function("strtold")
            .blocklist_function("_strtold_r")
            .clang_args(args)
            .clang_args(vec![
                "-target",
                if mcu == "esp32c3" {
                    "riscv32"
                } else {
                    "xtensa"
                },
            ]),
    )?;

    let c_incl_args = build::CInclArgs::try_from(&pio_scons_vars)?;

    cfg_args.propagate();
    cfg_args.output();

    if let Some(env_path) = link_args.as_ref().map(|_| pio_scons_vars.path.clone()) {
        cargo::set_metadata("EMBUILD_ENV_PATH", env_path);
    }

    let esp_idf = PathBuf::from(&pio_scons_vars.pio_framework_dir);
    cargo::set_metadata("EMBUILD_ESP_IDF_PATH", esp_idf.try_to_str()?);

    c_incl_args.propagate();

    if let Some(link_args) = link_args {
        link_args.propagate();
        link_args.output();
    }

    Ok(())
}
