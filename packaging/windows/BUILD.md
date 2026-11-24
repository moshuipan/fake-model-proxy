# Windows x64 打包

在 macOS ARM64 上准备交叉编译工具（需要 Xcode Command Line Tools 和 CMake/Ninja）：

```sh
rustup target add x86_64-pc-windows-msvc
rustup component add llvm-tools
cargo install cargo-xwin --version 0.23.1 --locked
```

在项目根目录执行以下命令。静态链接 C 运行库，映射本机路径，并移除调试符号：

```sh
XWIN_ARCH=x86_64 \
CL_FLAGS="/clang:-ffile-prefix-map=$HOME=/build/host" \
RUSTFLAGS="-C target-feature=+crt-static -C strip=symbols --remap-path-prefix=$HOME=/build/host" \
cargo xwin build --release --locked --target x86_64-pc-windows-msvc
```

先用 Rust 配套的 `llvm-readobj` 检查仅依赖 Windows 系统 DLL，再生成 ZIP：

```sh
"$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/llvm-readobj" \
  --coff-imports target/x86_64-pc-windows-msvc/release/fakemodelproxy.exe
python3 packaging/windows/package.py target/x86_64-pc-windows-msvc/release/fakemodelproxy.exe
```

ZIP 保存到项目父目录，包含 exe、start.cmd、中文说明、构建信息、项目许可证
和第三方依赖声明。对外发布二进制前，根据实际链接依赖补齐上游许可全文和 NOTICE；
依赖汇总表不能替代这些文件。
本机启动检查可执行 `cargo build --locked && python3 packaging/windows/smoke_local.py`。
发布前仍需在 Windows 上验证启动、账号登录及后端请求。

cargo-xwin 会使用系统 Clang，并从 Rust 工具链提供链接器和归档器。
如果系统 Clang 不支持项目中的 C 依赖，再安装独立的 LLVM 工具链。
