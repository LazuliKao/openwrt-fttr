# OpenWrt FTTR

H3C HM2004-DU 下联 Micro-OLT 的 OpenWrt feed。源码仓库：
https://github.com/LazuliKao/openwrt-fttr 。

## 目录

- `h3c-fmcs/`：内核驱动、FPGA 初始化、PLOAM/OMCI/GEM 通道及驱动测试。
- `h3c-fttr-tools/`：Rust 控制器 `fttrd`、命令入口 `fttrctl`、设备适配和可裁剪 ACS。

## 接入 PonWrt

在 OpenWrt 的 `feeds.conf.default` 或自定义 `feeds.conf` 中加入：

```text
src-git fttr https://github.com/LazuliKao/openwrt-fttr.git
```

在 PonWrt 根目录运行：

```sh
./scripts/feeds update -a
# 已有构建树先移除旧 feed 的两个包链接；不删除其他 PON 包。
./scripts/feeds uninstall h3c-fmcs h3c-fttr-tools
./scripts/feeds install -p fttr h3c-fmcs h3c-fttr-tools
./scripts/feeds install -a
make defconfig
make package/feeds/fttr/h3c-fmcs/compile V=s -j2
make package/feeds/fttr/h3c-fttr-tools/compile V=s -j2
```

`kmod-h3c-fmcs` 自动依赖 `h3c-fmcs-firmware`，镜像必须包含该包提供的
`/lib/firmware/FTTR_TOP.sbit`。缺少该文件会使 FPGA 初始化失败并暂停子光猫发现。

## 编译选项

| OpenWrt 配置 | Cargo feature | OpenWrt 默认 | Cargo 默认 |
| --- | --- | --- | --- |
| `CONFIG_FTTR_DEVICE_HL202` | `device-hl202` | 开 | 开 |
| `CONFIG_FTTR_DEVICE_HWTC` | `device-hwtc` | 开 | 关 |
| `CONFIG_FTTR_DEVICE_STAR` | `device-star` | 开 | 关 |
| `CONFIG_FTTR_DEVICE_ZTE` | `device-zte` | 开 | 关 |
| `CONFIG_FTTR_DEVICE_SCTY` | `device-scty` | 开 | 关 |
| `CONFIG_FTTR_ACS` | `acs` | 开 | 关 |
| `CONFIG_FTTR_MQTT` | `mqtt` | 开 | 开 |
| `CONFIG_FTTR_NPU` | `npu` | 开 | 开 |

OpenWrt 使用 `--no-default-features`，逐项显式映射上述选项。设备及 ACS
feature 不会隐式开启 MQTT；`all-devices` 组合上述全部设备模块。Linux 下在
`h3c-fttr-tools/src` 验证全部目标：

```sh
cargo check --locked --all-targets --no-default-features --features all-devices,acs,mqtt,npu
cargo check --locked --all-targets --no-default-features --features all-devices,acs,mqtt
cargo check --locked --all-targets --no-default-features --features all-devices,acs
```

### Workspace 与诊断工具

`h3c-fttr-tools/src/Cargo.toml` 是 workspace 根，所有内部 crate 共用根目录
`Cargo.lock`。普通 `cargo build` 只构建主包，默认二进制为 `fttrd` 和
`fttrctl`；OpenWrt 打包也显式限定这两个入口。完整主机测试在该目录执行：

```sh
cargo test --locked --workspace --features fttr-api/codegen
cargo test --locked --lib --no-default-features --features all-devices,acs,mqtt,npu
cargo check --locked --all-targets --no-default-features --features all-devices,acs,mqtt,npu,diagnostics
```

`fttr-platform-probe`、`fttr-omci-mib`、`fttr-omci-probe`、`fttr-omci-service`、
`fttr-ranging` 需显式启用 `diagnostics`；`fttr-k155-service` 还需 `device-hwtc`，
`fttr-star-observe` 还需 `device-star`。例如：

```sh
cargo build --locked --features diagnostics,device-star --bin fttr-star-observe
fttr-star-observe 02:00:00:00:00:01
```

STAR 观察工具要求传入源 MAC；不再内置现场设备地址。独立 ranging 工具仍需先停止
`fttrd`，OMCI MIB 入口复用控制器 inventory 核心。CLI 的受支持命令以
`fttrctl help` 为准，手动 `discover/register/auth/sync-wifi/provision` 入口已移除。

### 光口与 FEC 配置

光口策略统一位于 `optical` 节：`enabled` 和 `downstream_tx_enabled` 默认 `1`，
`fec_enabled` 默认 `0`。旧的 `provisioning.fec_enabled` 不再作为 FEC 开关；
升级后应核对并按需迁移到 `optical.fec_enabled`，前端也需要使用新的光口配置接口。

```sh
uci set fttr.optical.fec_enabled='1'  # 默认配置的节名；使用 0 关闭
uci commit fttr
fttrctl optical reload --json
fttrctl status --json
```

状态中的 `pon_mac` 报告期望设置和寄存器回读；寄存器位不等于已经验证光路纠错。
ONU class 312 下行 PM 是最近完成的 15 分钟区间，不是实时、累计或上行计数；
仍需独立验证真实 GEM 数据和光路行为。

`provisioning.allow_unsupported_as_bridge` 现在缺省开启，已有显式 `0` 仍保持关闭。
需要保持仅使用已适配设备的策略时，应显式设置为 `0`。

设备适配说明、测试清单和已验证边界集中在
[设备验证文档](docs/devices/README.md)，不在本文件逐型号追加现场记录。

## Windows Rust 交叉编译

在 `h3c-fttr-tools/src` 中使用已安装的 Rust target、Zig 和 cargo-zigbuild：

```powershell
$previous = $env:RUSTFLAGS
try {
    $env:RUSTFLAGS = '-C target-feature=-crt-static -C link-arg=-dynamic'
    cargo zigbuild --release --locked --target aarch64-unknown-linux-musl --no-default-features --features all-devices,acs,mqtt,npu --bin fttrd --bin fttrctl
} finally {
    $env:RUSTFLAGS = $previous
}
```

产物位于 `src/target/aarch64-unknown-linux-musl/release/`。
内核驱动与固件镜像使用 Linux OpenWrt 构建环境。

## LuCI SDK 生成

`fttr-api` 共享后端声明并生成 LuCI 类型和客户端，默认输出到
`../luci-app-fttr/src/api/generated/`。在仓库根目录执行：

```sh
cargo run --locked --manifest-path h3c-fttr-tools/src/crates/fttr-api/Cargo.toml --features codegen --bin fttr-sdk -- --generate
cargo run --locked --manifest-path h3c-fttr-tools/src/crates/fttr-api/Cargo.toml --features codegen --bin fttr-sdk -- --check
```

其他布局追加 `--out-dir FRONTEND/src/api/generated`。`--check` 只检查生成
一致性，不写文件。完整链路在前端运行 `pnpm sdk:generate`，自动生成
TypeScript 类型、客户端、ACL 和 Zod mini 校验；`pnpm check` 依次检查生成一致性。
