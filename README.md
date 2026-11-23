# OpenWrt FTTR

包含 H3C HM2004-DU Micro-OLT 平台组件及多厂商 FTTR 用户态控制器的 OpenWrt feed。  
源码仓库：<https://github.com/LazuliKao/openwrt-fttr>

## 仓库结构

- `h3c-fmcs/`：内核驱动、FPGA 初始化、PLOAM/OMCI/GEM 通道及驱动测试。
- `h3c-fmcs-firmware/`：H3C HM2004-DU Micro-OLT FPGA 固件包（`FTTR_TOP.sbit`）。
- `fttr/`：Rust FTTR 控制器 `fttrd`、CLI 工具 `fttrctl`、多厂商设备适配与可裁剪 ACS（用户态源码位于 `fttr/src/src/`）。

---

## 快速接入（PonWrt）

### 1. 添加 Feed 并编译

在 OpenWrt 的 `feeds.conf.default` 或自定义 `feeds.conf` 中加入：

```text
src-git fttr https://github.com/LazuliKao/openwrt-fttr.git
```

在 PonWrt 根目录执行：

```sh
./scripts/feeds update fttr
./scripts/feeds install -a -p fttr
make defconfig
make package/feeds/fttr/h3c-fmcs/compile V=s
make package/feeds/fttr/openwrt-fttr/compile V=s
```

### 2. LuCI 管理界面（独立仓库）

LuCI 包由 [luci-app-fttr](https://github.com/LazuliKao/luci-app-fttr) 独立维护，OpenWrt 包位于其 `package` 分支，可直接作为 feed（`src-git ...;package`）或克隆到 `package/` 下编译：

```sh
git clone --single-branch --branch package https://github.com/LazuliKao/luci-app-fttr.git package/luci-app-fttr
make defconfig
make package/luci-app-fttr/compile V=s
```

### 3. HM2004-DU 下行光校准

使用 [h3c-hm2004du-factory](https://github.com/qwe3017/h3c-hm2004du-factory) 导出 1MB `factory` 分区并刷入，或导出校准数据后在本插件校准页上传应用。

---

## 编译选项与开发指南

### OpenWrt 编译配置与 Cargo Features

<details>
<summary>展开查看网关平台与下游 ONU 编译开关对照表</summary>

#### 网关 / OLT 平台

| OpenWrt 配置 | Cargo feature | OpenWrt 默认 | Cargo 默认 | 状态 |
| --- | --- | --- | --- | --- |
| `CONFIG_FTTR_PLATFORM_H3C_HM2004_DU` | `platform-h3c-hm2004-du` | ✅ | ✅ | 依赖 H3C FMCS 驱动 |
| `CONFIG_FTTR_PLATFORM_FIBERHOME_AN4475_G2` | `platform-fiberhome-an4475-g2` | ❌ | ❌ | FSL 运行态、IPC 与 ubus 接口，依赖 `kmod-fiberhome-fsl`；分层验证范围见下文 |
| `CONFIG_FTTR_PLATFORM_TIANYI_TYFZA1` | `platform-tianyi-tyfza1` | ❌ | ❌ | 编译期 stub；板级身份及驱动尚未调查 |
| — | `all-platforms` | — | ❌ | Cargo 聚合 feature |

#### 下游 ONU 适配与服务

| OpenWrt 配置 | Cargo feature | OpenWrt 默认 | Cargo 默认 |
| --- | --- | --- | --- |
| `CONFIG_FTTR_DEVICE_HL202` | `device-hl202` | ✅ | ✅ |
| `CONFIG_FTTR_DEVICE_HWTC` | `device-hwtc` | ✅ | ❌ |
| `CONFIG_FTTR_DEVICE_STAR` | `device-star` | ✅ | ❌ |
| `CONFIG_FTTR_DEVICE_ZTE` | `device-zte` | ✅ | ❌ |
| `CONFIG_FTTR_DEVICE_SCTY` | `device-scty` | ✅ | ❌ |
| `CONFIG_FTTR_DEVICE_FIBERHOME` | `device-fiberhome` | ✅ | ❌ |
| `CONFIG_FTTR_DEVICE_REALTEK` | `device-realtek` | ✅ | ❌ |
| `CONFIG_FTTR_ACS` | `acs` | ✅ | ❌ |
| `CONFIG_FTTR_MQTT` | `mqtt` | ✅ | ✅ |
| `CONFIG_FTTR_NPU` | `npu` | ✅ | ✅ |
| `CONFIG_FTTR_MIMALLOC` | `allocator-mimalloc` | ✅ | ❌ |

</details>

### Workspace 测试与诊断工具

`fttr/src/Cargo.toml` 为 workspace 根，默认构建 `fttrd` 和 `fttrctl`。在 `fttr/src` 目录执行主机测试：

```sh
cargo test --locked --workspace --features fttr-api/codegen
cargo test --locked --lib --no-default-features --features platform-h3c-hm2004-du,all-devices,acs,mqtt,npu
cargo test --locked --lib --no-default-features --features platform-fiberhome-an4475-g2,all-devices,acs,mqtt
cargo check --locked --all-targets --no-default-features --features all-platforms,all-devices,acs,mqtt,npu,diagnostics
```

### AN4475-G2 遥测与业务门控

- 光控制器总开关、下行 TX 和下行 FEC 由唯一硬件 owner 应用，响应前核对 TX/FEC 寄存器；双平台共用完成确认、失败处理与周期回读机制，两平台的 `read_tx` 均返回寄存器实际值。硬件初始化失败时 AN4475-G2 有界重试，仍失败经 fault 通道交由 procd 重建进程（H3C 初始化失败立即退出，同样由 procd 重启）；检测到寄存器毛刺时 Status 会降级为 Disabled 并保持关闭，需显式 Reload 恢复。总开关是控制服务的提交状态，不代表独立 BOSA 复位或实测光功率；实际 TX/FEC 分别读取 `0x102` bit 4（反相）与 `0x202` bit 0。
- LuCI 将控制器、模块身份、校准状态与 GPON PHY 基本信息单独展示，不再依赖 HM2004 专用 FPGA 计数。UX3326S1 的 `0x6d/0x6f/0x71/0x73` 动态读数已作为 observed 证据暴露（含原始寄存器值与采样时间），换算采用 SFF-8472 惯例刻度但未做实机标定；CDR 锁定状态仍为未知。
- 硬件初始化假设 ONU-ID 等于 ONU 序号且 `alloc_offset_mode` 为 0；实机寄存器回读非 0 时拒绝启动并说明原因，避免静态资源表错配。上行 OMCI 应答按来源入队，仅接受已注册且处于 Active 的 ONU。DyingGasp 位图的硬件锁存/清除语义未验证，软件按"新置位"边沿判定且不写回寄存器。
- 光路校准使用独立 `ux3326s_factory` 能力分支：只读本机原厂 2 KiB 表，验证表头、载荷 CRC 和 `0x27` 芯片标识，展示来源、SHA-256 及本次 owner 的应用记录。重新应用先退役 ONU 业务并禁用下行，核验在线 `UX3326S1` 签名，再执行原厂 802 次寄存器写入计划；全部成功后恢复已保存的光学设置，写入失败保持下行关闭。不修改原厂分区，不提供 HM2004 系数编辑、上传或来源选择。应用记录表示受检 I²C 序列完成，不代表完整寄存器回读或实测光功率。
- LuCI 依据 `calibration_profile` 区分平台：HM2004 使用校准编辑器，AN4475-G2 的 `ux3326s_factory` 为只读原厂表展示；不支持校准或 NPU 的平台显示能力说明，不调用不支持的功能 RPC 或启动轮询。luci-app-fttr 需与本次 fttr-api 契约同步重新生成后才能解析新平台的校准与拓扑应答。
- 拓扑在确认主猫既无本地无线配置也无无线硬件时，将缺失的 `network.wireless.status` 标为 `not_applicable`；有无线或无法确定时仍保留数据源故障，不影响子猫无线采集。
- 自动 MIB 使用分段预算，保留上传序号和在途事务；活体 GET 与上传回复分别匹配。只有采集完整、注册代次仍有效且前后 MIB Data Sync 相同的清单才发布；8 位同步计数仍有回绕限制。
- 有界遥测提前刷新清单，保留有效期检查；自动采集不再每轮关闭业务口。独立监控线程在活体超龄或取消时关闭 VPORT，关闭失败继续重试并保留归属。用户态锁等待及 `ip`/`uci` 命令有超时，内核 ioctl 和系统调度不作硬实时保证。
- 显式 MIB/FEC/属性诊断会先关闭目标业务口，完成后须重新取得活体响应才开放。长诊断应安排在允许目标业务中断的窗口。
- AN4475-G2 的 `fttr1..fttr128` 接管前会检查服务别名、父接口、802.1ad 协议及 VID。旧版无 `fttrd:fiberhome-vport` 别名的接口需停服、确认归属后迁移；接口形状一致本身不是归属证据。
- 2026-11-20 的实机窗口验证了 HL202-DU-C、TY-FS803G、K155-11 的管理端点双向通信、自动清单刷新，以及 HL202 MQTT 命令/配置回读和整机重启恢复。提前刷新版三目标各 360 次 ping 无丢包；其后的审查修复版再次整机重启，三目标各 180 次 ping 无丢包，逐秒 carrier 未见断开、清单持续更新。HL202 VPORT 仍有少量 RX dropped 增量，原因未定位。真实无线客户端、吞吐和真实 CWMP RPC 需单独验收，不能由上述结果推定。
- 2026-11-21 的功能补齐版实机验证了 FEC 开/关、独立下行 TX 关/开、总开关关/开及对应寄存器回读；操作后恢复原配置，三目标各 120 次 ping 无丢包、逐秒 carrier 与进程未见异常，HL202 MQTT 命令及反向 ping 成功。无本地无线的数据源返回 `not_applicable`；LuCI 已部署基本硬件展示与平台化开关文案。HM2004 的平台专属计数器不属于 AN4475 已实现能力。
- 同日独立校准版实测启动应用及页面同源 `apply_calibration` 任务成功：原厂表校验通过，在线签名验证与 802 次受检写入完成，应用时间更新，原光学设置与配置哈希保持不变。重新注册后三目标各 120 次 ping 无丢包，逐秒 carrier 与进程稳定，HL202 MQTT 命令和反向 ping 成功；浏览器页面仍需刷新后确认显示。
- 整体审查补充了退役失败时仍尝试关闭 TX、校准截止与停止检查，以及 LuCI 重新应用按钮的事件、权限和任务失败回归。实机验证总开关关闭时重新应用校准成功且仍保持关闭，恢复原配置后三目标各 120 次 ping 无丢包，逐秒 carrier 与进程采样正常。

### Windows Rust 交叉编译

在 `fttr/src` 中使用已安装的 Rust target、Zig 和 `cargo-zigbuild`（内核驱动与完整固件仍需 Linux OpenWrt 环境）：

```powershell
$previous = $env:RUSTFLAGS
try {
    $env:RUSTFLAGS = '-C target-feature=-crt-static -C link-arg=-dynamic'
    cargo zigbuild --release --locked --target aarch64-unknown-linux-musl --no-default-features --features platform-h3c-hm2004-du,all-devices,acs,mqtt,npu,allocator-mimalloc --bin fttrd --bin fttrctl
} finally {
    $env:RUSTFLAGS = $previous
}
```

产物位于 `fttr/src/target/aarch64-unknown-linux-musl/release/`。

### LuCI SDK 生成

在仓库根目录执行 `fttr-sdk` 生成或校验前端接口定义（通过 `--out-dir` 指定目标路径）：

```sh
cargo run --locked --manifest-path fttr/src/crates/fttr-api/Cargo.toml --features codegen --bin fttr-sdk -- --generate --out-dir <OUT_DIR>
cargo run --locked --manifest-path fttr/src/crates/fttr-api/Cargo.toml --features codegen --bin fttr-sdk -- --check --out-dir <OUT_DIR>
```

---

## 开源协议

- `fttr/` 及仓库根目录通用内容采用 [Apache-2.0](LICENSE)（见 [fttr/LICENSE](fttr/LICENSE)）。
- `h3c-fmcs/` 内核驱动与配套测试代码采用 [GPL-2.0-only](h3c-fmcs/LICENSE)。
