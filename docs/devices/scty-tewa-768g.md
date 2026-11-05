# SCTY TEWA-768G：天翼原生管理引导

换机器接手先读[交接入口](handoff-20261105.md)，其中包含分支和未提交代码
恢复、设备备份、原厂证据重新提取命令及安全边界，不需要旧会话临时文件。

## 身份与范围

- 实机：TEWA-768G，硬件 V3.0，软件 Tianyi_V1.0.5，Broadcom BCM96878。
- 光侧 vendor 为 SCTY；CWMP ProductClass 为 TEWA-768G，
  ManufacturerOUI 为 8C8172。
- 此固件的 CWMP SerialNumber 是 `30861` 加 MAC，不是 GPON SN。
  对已观察到的 MAC 前缀 `8C81`，完整 CWMP SN 为
  `308618C81` 加 GPON SN 的后八位。只接受完整匹配，不采用任意后缀匹配。
- 身份兼容位于
  [CT 厂商模块](../../h3c-fttr-tools/src/src/devices/cwmp/vendor/ct.rs)，同时接入
  [光/CWMP 关联](../../h3c-fttr-tools/src/src/devices/mod.rs)和
  [CWMP 发现身份检查](../../h3c-fttr-tools/src/src/devices/cwmp/identity.rs)。
  身份关联不表示 WAN/Wi-Fi 写入已经验证，也不自动开通此型号的 OMCI 业务。

## 配置约定

沿用天翼固件，不手工把裸 `veip0` 接入 LAN 桥，不覆盖互联网、IPTV、
Wi-Fi、LOID 或运营商认证凭据。

1. 使用单设备 `adapter 'generic'` 绑定试验设备，禁止全局放行未知型号。
   通过现有配置模板机制提供空模板/组，不关联默认 Wi-Fi 模板。
2. 由通用 OMCI 实现动态发现 T-CONT 和 VEIP，开通光侧承载。
   首次完整库存为 240/240 条：8 个未绑定业务的 T-CONT、VEIP 0x0401，
   没有业务 GEM、桥接及 EVTO。成功试配使用 T-CONT 0x8000、Alloc-ID/GEM
   256；这些是实测结果，不是应硬编码的实例模板。
3. 本机管理 WAN 引导优先使用天翼原生 CMS/CWMP API。该机已有
   VLAN 46、优先级 7、ServiceList `TR069`、DHCP、IP_Routed、非 NAT 的
   管理连接；GPON LinkConfig Enable 初始为 0。实际接口创建由固件负责。
4. 主猫默认管理网络为 VLAN 46、192.168.46.1/24，ACS 地址为
   `http://192.168.46.1:7547/cwmp`。主猫网络、DHCP、防火墙及 ACS 必须
   分别回读就绪；不能以网桥存在或端口监听代替 Inform 验收。
5. 保持 `omci_management '0'`：本机库存未发现 ME 340，不能假设标准
   OMCI ACS 引导可用。首次本机引导与后续网络 TR-069 同步分别验收。

### 天翼原生只读发现

维护 shell 中已观察到的 D-Bus 服务、对象和接口：

```text
service: com.ctc.igd1
object:  /com/ctc/igd1/Telecom/CWMP
interface: com.ctc.igd1.CWMP

GetParameterNames(ObjectName, NextLevel)
GetParameterValues(List)
SetParameterValues(List)
AddObject(ObjectName)
DeleteObject(ObjectName)
```

首次引导前备份本机 PSI/PSI backup，并读取真实 WAN 实例，不能在批量设备
上照抄 `.1.`。GetParameterNames 返回可写性；该本机 GetParameterValues
返回字符串，不提供网络 CWMP 的 xsi:type，不能据此声称类型或写合同已验证。
所有调用都必须检查返回 result/errdesc，并在修改后回读。

该机管理 WAN 的实际参数：

```text
InternetGatewayDevice.WANDevice.<wan>.WANConnectionDevice.<parent>.
  X_CT-COM_WANGponLinkConfig.Enable
  X_CT-COM_WANGponLinkConfig.Mode
  X_CT-COM_WANGponLinkConfig.VLANIDMark
  X_CT-COM_WANGponLinkConfig.802-1pMark
  X_CT-COM_WANGponLinkConfig.VLANID
  WANIPConnection.<connection>.X_CT-COM_ServiceList
```

这与现有 `vendor-ct-candidate` 的 connection-local VLAN/priority 路径不同。
不能强制套用该候选，更不能把管理 ServiceList `TR069` 当作普通业务改写。
新增网络写合同前还需采集真实 Inform、参数类型、写入及回读证据。

## 测试清单与现场状态

| 项目 | 结果与证据 |
| --- | --- |
| 光注册 | O5；控制服务正常重启后重新注册成功 |
| 完整库存 | 首次 240/240 条；试配后 266/266 条，包含表数据 |
| OMCI 试配 | 完整业务回读成功，Alloc-ID 256，veip0 UP；新版控制器重启后再次达到 verified |
| EVTO | 三条透明默认规则，未观察到 F650A 的默认丢弃表，不套用其修复 |
| 原生配置恢复 | 光承载开通后，固件自动创建 VLAN 子接口及 LAN 桥关联 |
| 管理运行时 | 部署个人集成分支后 ready=true，ACS 监听管理地址；旧版报 cannot inspect command |
| 原生管理连接 Enable | 0→1，SetParameterValues result=0，后续回读为 1 |
| 原生 ACS URL | 已备份；改为主猫管理地址，result=0，回读一致；认证凭据未修改 |
| DHCP 与双向管理通道 | 未打通；GPON 软件接口可见 VLAN 46/PCP 7 的 DHCP 请求，但观察窗口内业务 GEM 发送计数不增长 |
| 上行调度 | 业务 T-CONT 有积压；连续 BWmap 样本仅给 Alloc-ID 256 两字节、DBRu=1 的授权，未见数据空间恢复 |
| 真实 Inform 与身份关联 | 未验证；源码支持严格原生 SN 关联，仍需网络实测 |
| 网络参数类型及写合同 | 未验证；候选仍禁止自动写入 |
| Wi-Fi/WAN 配置同步 | 未验证，未执行 |
| 子猫重启后保持 | 未验证，未重启子猫 |
| 回退 | 已备份主猫配置/旧控制器及子猫 PSI；未执行回退 |

原生管理 WAN 的 `Reset` 可写，单连接重连返回 result=0，Reset 自动回到 0；
ConnectionStatus 为 Connecting、ExternalIPAddress 为 0.0.0.0。
重连期间主猫 `fttr1` 和 `br-tr069` 的 30 秒被动观察未见 DHCP。
这些证据尚不能区分子猫光侧映射、VLAN 转发或其他承载问题；
不能把 ACS 监听成功当作子猫已接管，也不能仅凭短时抓包认定永久无流量。

### 当前卡点：上行数据授权

继续排查后，不能再仅归因于 DHCP 保留旧租约：

- 原生 `vlanctl --rule-dump-all` 的输出位于内核日志，而不是标准输出。
  `veip0.1` 的发送规则添加 VID 46、PCP 7，规则有命中；GPON 软件接口
  `gpon0.0` 抓到该管理 MAC 发出的 VLAN 46/PCP 7 DHCP 广播。
- 硬件 `gem/index=1` 对应 GEM 256，绑定 `tcont/index=1`；
  原生 PON 表显示业务 Alloc-ID 256 已同时收到分配和配置，
  不应把 BDMF 索引、PON 队列索引、OMCI 实例号混用。
- 管理续租前后 GEM TX 保持 204 帧、36129 字节；发送队列积压则从
  queue 0 的 35 帧/8332 字节增加至 47 帧/11944 字节，
  queue 7 另有 1 帧/82 字节。软件 TX 计数不等于帧已发上光纤。
- 原生带宽记录器按 `specific_access 256` 采到连续超帧的有效样本，
  均为 `Start=2, Stop=3, DBRu=1`。该两字节授权不足以发送 DHCP 数据；
  需继续定位主猫 DBA/DBRu 交互，而非继续修改 ACS URL 或认证参数。
  尚不能断言是 FPGA 算法缺陷，或子猫报告/调度兼容性问题。
- 主猫同一观察窗口的 `fttr1`、`br-tr069` 未见 DHCP，子猫仍无管理 IPv4；
  O5 和 OMCI `verified` 不能作为数据承载已经打通的依据。

只读硬件对象检查示例：

```sh
bs /Bdmf/ Examine gem/index=1
bs /Bdmf/ Examine egress_tm/dir=us,index=1 queue_occupancy
bs /Bdmf/ Examine gpon tcont_alloc_id
bs /Driver/pOn_stack/ ptcnt
```

上述索引均为本机发现结果，不能照搬到其他设备。原生带宽记录仅限有界
维护诊断，完成后已停止；临时 RX 带宽计数器关联已恢复原值。
默认四个 RX 带宽计数器均关联 Alloc-ID 0，因此未先核对关联时，
其他 T-CONT 显示零不能证明它从未收到授权。记录器的未填充条目也不能
当作真实错误 BWmap。现有公开源码未提供经验证的 FPGA DBA 配置接口，
不以猜测地址写寄存器、手工替换业务 Alloc-ID 或绕过 ONU 锁作为修复。

已核查
[openwrt-hm2004-feed](https://github.com/rayniel/openwrt-hm2004-feed)：
其 `original/docs/MOLT_API.md` 记录了原厂 `moltdbg fpga bandwidth`、
`burst`、上下行使用率等诊断入口，是继续定位 DBA 的线索；
但仓库没有原厂 `moltdbg`、`moltd`、`libminiolt_svc.so` 二进制，
没有恢复 DBA 寄存器布局，也没有发布附件补充这些材料。
其中 `original/userspace/miniolt.c` 是重建的通用寄存器/控制通道工具，
不能当作已经恢复了原厂带宽算法。

### 原厂备份交叉核对

后续取得
[hm2004-fttr-research 原始备份](https://github.com/rayniel/hm2004-fttr-research/tree/814878f473ea499a674a6d63ca675b0f3b3e4dc9/artifacts/original/1c94686f5178)。
仅在本地静态分析 rootfs 中的程序和依赖库，没有在设备上运行这些原厂
二进制、替换 FPGA 固件或恢复整机配置。opt0 是嵌套的 OpenWrt 用户环境，
未发现额外的 miniOLT 板级程序。

`moltdbg` 的带宽显示函数（虚拟地址 `0x401490`）直接读取以下寄存器；
带宽取低 20 位、权重取位 24..30、使能取控制寄存器位 28。
ONU 0 的现场回读如下，其他 ONU 的四个地址均增加 `16 * ONU-ID`：

| 项目 | 地址 | 现场值 | 解码 |
| --- | --- | --- | --- |
| OMCI 带宽/权重 | `0x05000098` | `0x0400003c` | 带宽字段 60、权重 4 |
| OMCI 使能 | `0x0500009c` | `0x11000000` | 已使能 |
| DATA 带宽/权重 | `0x050000a0` | `0x04004bc5` | 带宽字段 19397、权重 4 |
| DATA 使能 | `0x050000a4` | `0x11000000` | 已使能 |

上述带宽是寄存器字段值，尚未验证单位，不能直接标成 Mbps。
原厂工具还以 `0x00000020 == 0x00017258` 判断主带宽分配开启；
现场值符合该判断，ONU 使能位图 `0x01000008` 为 1。
因此不能把问题描述成“业务带宽寄存器未配置/未使能”，也不能据此
继续盲目增大带宽。

真正调用 `MiniOltWriteFpgaReg` 的原厂 `ploam` 包含
`PloamGponSetAllocIdVaild`（`0x410580`）：启用时写入 `0x11000000`，
与当前控制器使用的值一致。该接口的首参数是抽象寄存器编号，
由 `libminiolt.so` 的表转换成原始地址，不能直接作为 FPGA 地址使用。
其默认上行开销和扩展突发 PLOAM 模板也与当前实现一致：
`20 00 00 aa ab 59 83 00 00 00`、`40 40`。
这只能排除这些已核对项的差异，不代表完整的调度契约已恢复。

备份中的另一套 `moltd` 虽有 `subGponTcontCfgSetBw` 等符号，
但使用不同的 SPI/间接寄存器后端；它的 `0x6a00` 表布局不能套用到
本机 miniOLT 的 FMCS 寄存器接口。`tlut` 是 BOSA 参数工具，不是 DBA
配置工具。

补充现场验证：

- `all_accesses` 有界记录显示，同一超帧连续出现 Alloc-ID 0 的
  `Start=0, Stop=1, DBRu=1` 和 Alloc-ID 256 的
  `Start=2, Stop=3, DBRu=1`，多帧 CRC 有效；随后已停止记录器。
  这是报告尺寸的授权，不是能够承载 DHCP 的数据授权。
- 子猫业务 GEM TX 仍为 204 帧/36129 字节，queue 0 积压增加至
  109 帧/30606 字节，queue 7 仍为 1 帧/82 字节；
  `veip0.1` 只有链路本地 IPv6，没有 IPv4。
- 原生 TX PM 两次读取均显示 `illegal_access_counter=65535`；
  尚未确认计数清除/饱和语义以及对应的 Alloc-ID，不能直接认定
  上述业务授权全部非法。FIFO 汇总发送计数也不能替代业务 GEM 的增量。
- 按原厂工具映射读取的上行组包错误寄存器 `0x0201000c` 至
  `0x02010024` 均为零；这不等于帧已发出，也不证明 DBRu 被正确接收。

后续仍需确认 DBRu 接收、报告解释及授权长度之间的关系，或取得同硬件
原厂正常工作时的对照。没有这一证据前，不修改共享调度模式、不注入
猜测的固定授权，不把 O5、OMCI 成功或寄存器回读当作兼容性验收通过。
修复验收必须同时看到业务 GEM TX 增长、DHCP 完成交互、管理 IPv4
建立以及 ACS 收到真实 Inform。

主猫控制器升级复用当前个人分支已合入的上游 ubus SIGCHLD 修复，
未通过忽略 ECHILD 或伪造命令成功掩盖运行时故障。
OMCI 启动曾返回 MIB count=0，后续正常上传成功；被动探针观察到
Upload-NEXT 请求/响应连续推进，约一秒一条。诊断必须保留正常 ONU
锁和有界等待，不能删除锁或并发绕过守护进程。

源码验收必须覆盖原生完整 SN、错误型号/厂商/前缀/长度、另一台设备 SN、
generic 关联和 CWMP discovery 两个入口，以及关闭设备 features 后的行为。
能力验收仍以真实 Inform、类型发现、目标字段写入回读和重启保持为准。

本次源码验证：

- `all-devices,acs,mqtt,npu` 的 `devices::` 库测试：110 通过，1 忽略。
- 禁用默认 features 的 `devices::` 库测试：25 通过，2 忽略。
- 完整 features 的 all-targets 检查及 OpenWrt 目标编译通过。
- 包含严格原生 SN 关联的新控制器已部署并回读 SHA-256：
  `fc693f8b7ba28ba8ea00ba7e2276cd571a18b6cf6aadf2a5b36ca62abf566caa`；
  进程运行，管理网络 ready=true，ACS 仍监听管理地址。

## 下一步清单（当前优先）

- [ ] 先只读复核 O5、业务 verified、管理 ready、子猫 IPv4、GEM256 TX
  和队列；这些都是历史状态，不能直接当作新现场测量。
- [ ] 在同一有界窗口对应软件 DHCP、实际 GEM TX、BWmap 和主猫接收；
  完成后停止记录器并恢复临时诊断关联，不影响其他 WAN。
- [ ] 找到 DBRu 接收/解释与授权长度的板级契约，或同硬件原厂正常对照；
  当前带宽非零，禁止凭猜测加带宽或套另一套 moltd 的寄存器。
- [ ] 确认 TX PM 非法访问计数的语义和归属，不能仅凭 65535 定根因。
- [ ] 精确修复方案必须包含原值、授权范围、硬件资源锁及回退；
  共享调度、固件、重启等变更另行取得用户授权。
- [ ] GEM TX 增长、DHCP DORA、管理 IPv4 和真实 Inform 全部通过后，
  实测原生完整 SN 关联，再发现 SOAP 参数类型及真实管理 WAN 实例。
- [ ] 保持 CT candidate 禁止自动写入；用户选择同步字段后才验证写回，
  最后授权测试重启保持及批量设备适用范围。
