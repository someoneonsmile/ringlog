// 构建脚本：生成 shell 补全文件和 man 手册页。
//
// 只有设置了 SHELL_HELP_DIR 环境变量时才生成到 $SHELL_HELP_DIR/complete/
// 与 $SHELL_HELP_DIR/man/；未设置时静默跳过（普通 cargo build 不在源码树里产生文件）。
include!("src/app/cli_types.rs");

use std::env;
use std::error::Error;
use std::fs::{File, create_dir_all};

use clap::CommandFactory;
use clap::ValueEnum;
use clap_complete::{Shell, generate_to};
use clap_mangen::Man;

fn main() -> Result<(), Box<dyn Error>> {
    // 补全内容由 clap 定义决定；这两个文件变了才需要重新生成。
    println!("cargo:rerun-if-changed=src/app/cli_types.rs");
    println!("cargo:rerun-if-changed=build.rs");
    // 环境变量本身也要触发重跑，否则先 cargo build 再 SHELL_HELP_DIR=... cargo build
    // 会命中缓存、不生成补全。
    println!("cargo:rerun-if-env-changed=SHELL_HELP_DIR");

    let outdir = match env::var_os("SHELL_HELP_DIR") {
        None => return Ok(()),
        Some(outdir) => std::path::PathBuf::from(outdir),
    };

    let mut cmd = Args::command();
    let bin_name = cmd.get_name().to_owned();

    // 为所有支持的 shell 生成补全文件（bash/zsh/fish/elvish/powershell）
    let complete_dir = outdir.join("complete");
    create_dir_all(&complete_dir)?;
    for &shell in Shell::value_variants() {
        generate_to(shell, &mut cmd, &bin_name, &complete_dir)?;
    }

    // 生成 man 手册页（roff 源码，安装到 man1）
    let man_dir = outdir.join("man");
    create_dir_all(&man_dir)?;
    let man = Man::new(cmd);
    let mut man_out = File::create(man_dir.join(format!("{bin_name}.1")))?;
    man.render(&mut man_out)?;

    Ok(())
}
