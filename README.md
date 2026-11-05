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

正常保留配置的 sysupgrade 会同时备份 `/lib/firmware/modified_UX3326.bin`
校准文件（若存在），保留项由 `/lib/upgrade/keep.d/fttr` 声明。

ZTE F650A（OMCI EquipmentId 为 `ZXHN F650A(GPON ONU)`）使用独立型号适配，
从 MIB 发现 T-CONT 和 VEIP。仅对精确匹配的三条出厂 EVTO 丢弃规则替换为
透明转发，写后必须回读一致；遇到其它既有规则拒绝覆盖。该适配不声明支持
F650A 的 CWMP/Wi-Fi 配置，业务转发仍需用真实流量另行验证。

F650A V3.0.0P1T1 的本机配置还可能启用厂商 ARP 接管：
`/proc/net/arp_stolen` 为 `1` 时，OMCI/GEM 下行正常也可能无法建立后挂
LAN 设备的 ARP 邻居。先用受控抓包和真实双向流量确认，备份子光猫配置后，
通过完整维护会话检查 `sendcmd 1 DB p LAND` 中的 `ArpStolenEnable`。
该固件的内核运行开关是全局的：仅关闭主 LAN 的字段，另一 LAN 的开启值
仍会在重启时恢复接管。经确认允许变更的 LAN 行应设为 `0`，再执行
`sendcmd 1 DB save` 和 `sync`；重启后必须核对运行开关及双向业务流量。
这属于子光猫本机配置，不由 OMCI 型号适配自动修改，也不等于开放 PON/WAN
侧本机管理。固定 LAN telnet 的完整维护权限还需保存
`TelnetCfg.InitSecLvl=2`，并重新登录验证配置读写与诊断命令；保持
`Wan_Enable=0`，不要把临时维护账号的权限误认为固定账号已有权限。

持久化状态按内容去重：管理网络 journal 在成功提交后缓存快照，相同状态不再
写盘；共用原子写入层也避免重写相同文件。事务状态变更仍同步落盘，失败重试
保留文件和目录同步屏障，以保证断电恢复。


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
