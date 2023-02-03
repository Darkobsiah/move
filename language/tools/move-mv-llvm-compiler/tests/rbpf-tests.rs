use anyhow::Context;
use extension_trait::extension_trait;
use solana_rbpf as rbpf;
use std::path::{Path, PathBuf};
use std::process::Command;

mod test_common;
use test_common as tc;

pub const TEST_DIR: &str = "tests/rbpf-tests";

datatest_stable::harness!(run_test, TEST_DIR, r".*\.move$");

fn run_test(test_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Ok(run_test_inner(test_path)?)
}

fn run_test_inner(test_path: &Path) -> anyhow::Result<()> {
    let sbf_tools = get_sbf_tools()?;
    let runtime = get_runtime(&sbf_tools)?;

    let harness_paths = tc::get_harness_paths()?;
    let test_plan = tc::get_test_plan(test_path)?;

    if test_plan.should_ignore() {
        eprintln!("ignoring {}", test_plan.name);
        return Ok(());
    }

    tc::run_move_build(&harness_paths, &test_plan)?;

    let compilation_units = tc::find_compilation_units(&test_plan)?;

    compile_all_bytecode_to_object_files(&harness_paths, &compilation_units)?;

    let exe = link_object_files(&test_plan, &sbf_tools, &compilation_units, &runtime)?;

    run_rbpf(&exe)?;

    Ok(())
}

#[extension_trait]
impl CompilationUnitExt for tc::CompilationUnit {
    fn object_file(&self) -> PathBuf {
        self.bytecode.with_extension("o")
    }
}

fn compile_all_bytecode_to_object_files(
    harness_paths: &tc::HarnessPaths,
    compilation_units: &[tc::CompilationUnit],
) -> anyhow::Result<()> {
    tc::compile_all_bytecode(harness_paths, compilation_units, "-O", &|cu| {
        cu.object_file()
    })
}

struct SbfTools {
    _root: PathBuf,
    clang: PathBuf,
    rustc: PathBuf,
    cargo: PathBuf,
    lld: PathBuf,
}

fn get_sbf_tools() -> anyhow::Result<SbfTools> {
    let sbf_tools_root =
        std::env::var("SBF_TOOLS_ROOT").context("env var SBF_TOOLS_ROOT not set")?;
    let sbf_tools_root = PathBuf::from(sbf_tools_root);

    let sbf_tools = SbfTools {
        _root: sbf_tools_root.clone(),
        clang: sbf_tools_root
            .join("llvm/bin/clang")
            .with_extension(std::env::consts::EXE_EXTENSION),
        rustc: sbf_tools_root
            .join("rust/bin/rustc")
            .with_extension(std::env::consts::EXE_EXTENSION),
        cargo: sbf_tools_root
            .join("rust/bin/cargo")
            .with_extension(std::env::consts::EXE_EXTENSION),
        lld: sbf_tools_root.join("llvm/bin/ld.lld"),
    };

    if !sbf_tools.clang.exists() {
        anyhow::bail!("no clang bin at {}", sbf_tools.clang.display());
    }
    if !sbf_tools.rustc.exists() {
        anyhow::bail!("no rustc bin at {}", sbf_tools.rustc.display());
    }
    if !sbf_tools.cargo.exists() {
        anyhow::bail!("no cargo bin at {}", sbf_tools.cargo.display());
    }
    if !sbf_tools.lld.exists() {
        anyhow::bail!("no lld bin at {}", sbf_tools.lld.display());
    }

    Ok(sbf_tools)
}

struct Runtime {
    /// The path to the Rust staticlib (.a) file
    archive_file: PathBuf,
}

fn get_runtime(sbf_tools: &SbfTools) -> anyhow::Result<Runtime> {

    static BUILD: std::sync::Once = std::sync::Once::new();

    BUILD.call_once(|| {
        eprintln!("building move-native runtime for sbf");

        // release mode required to eliminate large stack frames
        let res = sbf_tools.run_cargo(&[
            "build", "-p", "move-native",
            "--target", "sbf-solana-solana",
            "--release",
        ]);

        if let Err(e) = res {
            panic!("{e}");
        }
    });

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo manifest dir");
    let manifest_dir = PathBuf::from(manifest_dir);
    let archive_file = manifest_dir
        .join("../../../")
        .join("target/sbf-solana-solana/")
        .join("release/libmove_native.a");

    if !archive_file.exists() {
        anyhow::bail!("native runtime not found at {archive_file:?}. this is a bug");
    }

    Ok(Runtime {
        archive_file,
    })
}

impl SbfTools {
    fn run_cargo(&self, args: &[&str]) -> anyhow::Result<()> {
        let mut cmd = Command::new(&self.cargo);
        cmd.env_remove("RUSTUP_TOOLCHAIN");
        cmd.env("CARGO", &self.cargo);
        cmd.env("RUSTC", &self.rustc);
        cmd.args(args);

        let status = cmd.status()?;
        if !status.success() {
            anyhow::bail!("running SBF cargo failed");
        }

        Ok(())
    }
}

fn link_object_files(
    test_plan: &tc::TestPlan,
    sbf_tools: &SbfTools,
    compilation_units: &[tc::CompilationUnit],
    runtime: &Runtime,
) -> anyhow::Result<PathBuf> {
    let link_script = {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo manifest dir");
        let manifest_dir = PathBuf::from(manifest_dir);
        let link_script = manifest_dir.join("tests/sbf-link-script.ld");
        link_script.to_string_lossy().to_string()
    };

    let output_dylib = test_plan.build_dir.join("output.so");

    let mut cmd = Command::new(&sbf_tools.lld);
    cmd.arg("--threads=1");
    cmd.arg("-znotext");
    cmd.arg("-znoexecstack");
    cmd.args(&["--script", &link_script]);
    cmd.arg("--gc-sections");
    cmd.arg("-shared");
    cmd.arg("--Bstatic");
    cmd.args(["--entry", "main"]);
    cmd.arg("-o");
    cmd.arg(&output_dylib);

    for cu in compilation_units {
        cmd.arg(&cu.object_file());
    }

    cmd.arg(&runtime.archive_file);

    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "linking with lld failed. stderr:\n\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(output_dylib)
}

fn run_rbpf(exe: &Path) -> anyhow::Result<()> {
    use rbpf::ebpf;
    use rbpf::elf::Executable;
    use rbpf::memory_region::MemoryRegion;
    use rbpf::verifier::RequisiteVerifier;
    use rbpf::vm::*;
    use std::sync::Arc;

    let elf = &std::fs::read(exe)?;
    let mem = &mut vec![0; 1024];

    let config = Config {
        dynamic_stack_frames: false,
        enable_elf_vaddr: false,
        reject_rodata_stack_overlap: false,
        static_syscalls: false,
        enable_instruction_meter: false,
        ..Config::default()
    };
    let loader = Arc::new(BuiltInProgram::new_loader(config));
    let executable = Executable::<TestContextObject>::from_elf(elf, loader).unwrap();
    let mem_region = MemoryRegion::new_writable(mem, ebpf::MM_INPUT_START);
    let verified_executable =
        VerifiedExecutable::<RequisiteVerifier, TestContextObject>::from_executable(executable)
            .unwrap();
    let mut context_object = TestContextObject::new(1);
    let mut vm = EbpfVm::new(
        &verified_executable,
        &mut context_object,
        &mut [],
        vec![mem_region],
    )
    .unwrap();

    let (_instruction_count, result) = vm.execute_program(true);

    let result = Result::from(result);

    match result {
        Ok(0) => {}
        Ok(_) => {
            // fixme rbpf expects a function that returns a status code, but we
            // currently emit a main function that returns void, so this value
            // is seemingly whatever happens to be in the return register.
        }
        e => {
            panic!("{e:?}");
        }
    }

    Ok(())
}
