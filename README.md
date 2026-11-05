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

SCTY 仅精确匹配 `COMHEART TY-FS803G`，复用按 MIB 发现资源的标准 OMCI
桥接流程。当前为桥接适配入口；双向 GEM、管理网络与 CWMP/Wi-Fi 尚待独立
现场验收，不包含推测的 CWMP 串号规则或私有参数路径。

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

### FEC 诊断（实验性）

全局开关位于 LuCI **配置 → 基本设置 → Enable FEC support (experimental)**，
对应 provisioning 节的 `fec_enabled`，默认 `0`（旧配置缺省也关闭）。保存并应用后通过
现有配置热加载生效，无需重新注册 ONU。启用后允许后台下行 PM 采集和显式诊断；
关闭后不再发起 FEC PM GET/CREATE，并隐藏旧的统计缓存，不删除已经存在的 PM 实体。
该开关启用的是统计支持，不代表 FPGA 编解码器开启；上下行硬件启用状态仍为未知。

```sh
uci set fttr.provisioning.fec_enabled='1'  # 默认配置的节名；使用 0 关闭
uci commit fttr
fttrctl config reload
```

```sh
fttrctl status --json          # sub_gateways[].fec：当前代次的状态与证据
fttrctl fec 0 --json           # 对当前 ONU 0 串行执行标准下行 PM GET
fttrctl fec 0 --create-pm       # 显式允许创建缺失的 class 312 PM 实例
```

先用状态确认当前 ONU ID；CLI 将请求绑定到串号、注册代次和已发现的 ANI-G。
后台最多增加一次 2 秒 GET，失败后退避 5 分钟；不影响活体判定，也不自动创建实体。
`--create-pm` 仅在 GET 返回 unknown instance（结果 5）时 CREATE，阈值指针为 0；
不重置已有 PM，不改既有阈值，不自动删除创建的 PM。返回 `created_pm` 标明是否创建，
后续正常遥测会读取它。若设备拒绝该语义，保留原始结果码，不按成功处理。

class 312 的 GET 是**最近完成的 15 分钟下行 PM 区间**，不是实时、累计或上行计数；
区间结束前可能没有有效的新结果。实例缺失、拒绝或超时不等于 ONU 不支持 FEC。
`control_supported=false` 表示当前软件尚无已验证的 FPGA 控制路径；
`downstream_enabled` / `upstream_enabled` 保持 `null`，计数为零也不推断开关关闭。
当前没有 FEC enable 命令：库存固件的抽象寄存器号尚未映射到本板 FPGA 原始地址。

正常保留配置的 sysupgrade 会同时备份 `/lib/firmware/modified_UX3326.bin`
校准文件（若存在），保留项由 `/lib/upgrade/keep.d/fttr` 声明。

设备适配说明、测试清单和已验证边界集中在
[设备验证文档](docs/devices/README.md)，不在本文件逐型号追加现场记录。

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
