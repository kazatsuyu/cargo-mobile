use crate::{config::Config, env::Env, ndk};
use ginit_core::{
    cargo::DotCargoTarget,
    config::ConfigTrait,
    opts::{NoiseLevel, Profile},
    target::TargetTrait,
    util::{self, ln},
};
use into_result::{command::CommandError, IntoResult as _};
use std::{collections::BTreeMap, fmt, fs, io, path::PathBuf, str};

fn so_name(config: &Config) -> String {
    format!("lib{}.so", config.shared().app_name())
}

#[derive(Clone, Copy, Debug)]
pub enum CargoMode {
    Check,
    Build,
}

impl fmt::Display for CargoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CargoMode::Check => write!(f, "check"),
            CargoMode::Build => write!(f, "build"),
        }
    }
}

impl CargoMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CargoMode::Check => "check",
            CargoMode::Build => "build",
        }
    }
}

#[derive(Debug)]
pub enum CompileLibError {
    MissingTool(ndk::MissingToolError),
    CargoFailed {
        mode: CargoMode,
        cause: CommandError,
    },
}

impl fmt::Display for CompileLibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileLibError::MissingTool(err) => write!(f, "{}", err),
            CompileLibError::CargoFailed { mode, cause } => {
                write!(f, "`Failed to run `cargo {}`: {}", mode, cause)
            }
        }
    }
}

#[derive(Debug)]
pub enum LibSymlinkError {
    JniLibsSubDirCreationFailed(io::Error),
    SourceMissing { src: PathBuf },
    SymlinkFailed(ln::Error),
}

impl fmt::Display for LibSymlinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LibSymlinkError::JniLibsSubDirCreationFailed(err) => {
                write!(f, "Failed to create \"jniLibs\" subdirectory: {}", err)
            }
            LibSymlinkError::SourceMissing { src } => write!(
                f,
                "The symlink source is {:?}, but nothing exists there.",
                src
            ),
            LibSymlinkError::SymlinkFailed(err) => write!(f, "Failed to symlink lib: {}", err),
        }
    }
}

#[derive(Debug)]
pub enum BuildError {
    BuildFailed(CompileLibError),
    LibSymlinkFailed(LibSymlinkError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::BuildFailed(err) => write!(f, "Build failed: {}", err),
            BuildError::LibSymlinkFailed(err) => write!(f, "Failed to symlink built lib: {}", err),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Target<'a> {
    pub triple: &'a str,
    clang_triple_override: Option<&'a str>,
    binutils_triple_override: Option<&'a str>,
    pub abi: &'a str,
    pub arch: &'a str,
}

impl<'a> TargetTrait<'a> for Target<'a> {
    const DEFAULT_KEY: &'static str = "aarch64";

    fn all() -> &'a BTreeMap<&'a str, Self> {
        lazy_static::lazy_static! {
            static ref TARGETS: BTreeMap<&'static str, Target<'static>> = {
                let mut targets = BTreeMap::new();
                targets.insert("aarch64", Target {
                    triple: "aarch64-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "arm64-v8a",
                    arch: "arm64",
                });
                targets.insert("armv7", Target {
                    triple: "armv7-linux-androideabi",
                    clang_triple_override: Some("armv7a-linux-androideabi"),
                    binutils_triple_override: Some("arm-linux-androideabi"),
                    abi: "armeabi-v7a",
                    arch: "arm",
                });
                targets.insert("i686", Target {
                    triple: "i686-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "x86",
                    arch: "x86",
                });
                targets.insert("x86_64", Target {
                    triple: "x86_64-linux-android",
                    clang_triple_override: None,
                    binutils_triple_override: None,
                    abi: "x86_64",
                    arch: "x86_64",
                });
                targets
            };
        }
        &*TARGETS
    }

    fn triple(&'a self) -> &'a str {
        self.triple
    }

    fn arch(&'a self) -> &'a str {
        self.arch
    }
}

impl<'a> Target<'a> {
    fn clang_triple(&self) -> &'a str {
        self.clang_triple_override.unwrap_or_else(|| self.triple)
    }

    fn binutils_triple(&self) -> &'a str {
        self.binutils_triple_override.unwrap_or_else(|| self.triple)
    }

    pub fn for_abi(abi: &str) -> Option<&'a Self> {
        Self::all().values().find(|target| target.abi == abi)
    }

    pub fn generate_cargo_config(
        &self,
        config: &Config,
        env: &Env,
    ) -> Result<DotCargoTarget, ndk::MissingToolError> {
        let ar = env
            .ndk
            .binutil_path(ndk::Binutil::Ar, self.binutils_triple())?
            .display()
            .to_string();
        // Using clang as the linker seems to be the only way to get the right library search paths...
        let linker = env
            .ndk
            .compiler_path(
                ndk::Compiler::Clang,
                self.clang_triple(),
                config.min_sdk_version(),
            )?
            .display()
            .to_string();
        Ok(DotCargoTarget {
            ar: Some(ar),
            linker: Some(linker),
            rustflags: vec![
                "-Clink-arg=-landroid".to_owned(),
                "-Clink-arg=-llog".to_owned(),
                "-Clink-arg=-lOpenSLES".to_owned(),
            ],
        })
    }

    fn compile_lib(
        &self,
        config: &Config,
        env: &Env,
        noise_level: NoiseLevel,
        profile: Profile,
        mode: CargoMode,
    ) -> Result<(), CompileLibError> {
        let min_sdk_version = config.min_sdk_version();
        util::CargoCommand::new(mode.as_str())
            .with_verbose(noise_level.is_pedantic())
            .with_package(Some(config.shared().app_name()))
            .with_manifest_path(Some(config.shared().manifest_path()))
            .with_target(Some(self.triple))
            .with_features(Some("vulkan")) // TODO: rust-lib plugin
            .with_no_default_features(true)
            .with_release(profile.is_release())
            .into_command(env)
            .env("ANDROID_NATIVE_API_LEVEL", min_sdk_version.to_string())
            .env(
                "TARGET_AR",
                env.ndk
                    .binutil_path(ndk::Binutil::Ar, self.binutils_triple())
                    .map_err(CompileLibError::MissingTool)?,
            )
            .env(
                "TARGET_CC",
                env.ndk
                    .compiler_path(ndk::Compiler::Clang, self.clang_triple(), min_sdk_version)
                    .map_err(CompileLibError::MissingTool)?,
            )
            .env(
                "TARGET_CXX",
                env.ndk
                    .compiler_path(ndk::Compiler::Clangxx, self.clang_triple(), min_sdk_version)
                    .map_err(CompileLibError::MissingTool)?,
            )
            .status()
            .into_result()
            .map_err(|cause| CompileLibError::CargoFailed { mode, cause })
    }

    pub(super) fn get_jnilibs_subdir(&self, config: &Config) -> PathBuf {
        config
            .project_path()
            .join(format!("app/src/main/jniLibs/{}", &self.abi))
    }

    fn make_jnilibs_subdir(&self, config: &Config) -> Result<(), io::Error> {
        let path = self.get_jnilibs_subdir(config);
        fs::create_dir_all(path)
    }

    pub(super) fn clean_jnilibs(config: &Config) -> io::Result<()> {
        for target in Self::all().values() {
            let link = target.get_jnilibs_subdir(config).join(so_name(config));
            if let Ok(path) = fs::read_link(&link) {
                if !path.exists() {
                    log::info!(
                        "deleting broken symlink {:?} (points to {:?}, which doesn't exist)",
                        link,
                        path
                    );
                    fs::remove_file(link)?;
                }
            }
        }
        Ok(())
    }

    fn symlink_lib(&self, config: &Config, profile: Profile) -> Result<(), LibSymlinkError> {
        self.make_jnilibs_subdir(config)
            .map_err(LibSymlinkError::JniLibsSubDirCreationFailed)?;
        let so_name = so_name(config);
        let src = config.shared().prefix_path(format!(
            "target/{}/{}/{}",
            &self.triple,
            profile.as_str(),
            &so_name
        ));
        if src.exists() {
            let dest = self.get_jnilibs_subdir(config).join(&so_name);
            ln::force_symlink(src, dest, ln::TargetStyle::File)
                .map_err(LibSymlinkError::SymlinkFailed)
        } else {
            Err(LibSymlinkError::SourceMissing { src })
        }
    }

    pub fn check(
        &self,
        config: &Config,
        env: &Env,
        noise_level: NoiseLevel,
    ) -> Result<(), CompileLibError> {
        self.compile_lib(config, env, noise_level, Profile::Debug, CargoMode::Check)
    }

    pub fn build(
        &self,
        config: &Config,
        env: &Env,
        noise_level: NoiseLevel,
        profile: Profile,
    ) -> Result<(), BuildError> {
        self.compile_lib(config, env, noise_level, profile, CargoMode::Build)
            .map_err(BuildError::BuildFailed)?;
        self.symlink_lib(config, profile)
            .map_err(BuildError::LibSymlinkFailed)
    }
}