# ZTE F650A 适配说明与测试清单

## 身份与实现

- 实机硬件 V4.0，固件 V3.0.0P1T1；其它版本未验证。
- OMCI EquipmentId 精确匹配 `ZXHN F650A(GPON ONU)`，同时核对 ZTE 厂商身份。
- [型号实现](../../h3c-fttr-tools/src/src/devices/zte/f650a/mod.rs)
  复用通用 MIB 发现和桥接流程，提供型号专用 EVTO 操作。
- 此模块不是完整 FTTR/CWMP 统一管理适配，不声明 Wi-Fi 参数写入支持。

## 配置行为与限制

仅接受空 EVTO 表、精确匹配的三条已知出厂丢弃规则或已知透明规则。
已知丢弃规则替换为透明规则并完整回读；透明规则幂等接受；
其它既有规则报错，不覆盖。T-CONT 与 VEIP 从 MIB 发现。

本固件还有厂商 ARP 接管：`/proc/net/arp_stolen=1` 时，正常到达
GEM/桥入口的 ARP 仍可能无法建立后挂 LAN 邻居。正式本机字段为
`LAND.ArpStolenEnable`。内核运行开关是全局的：仅关闭主 LAN 行，
另一 LAN 的开启值仍会在重启时恢复接管。

这不是 OMCI 模块自动修改的参数。只有备份配置、确认影响范围并取得授权后，
才可对相关 LAN 行设为 `0`，执行 `sendcmd 1 DB save`、`sync`，
重启核对运行开关和实际双向业务。不能把此操作等同于开放 PON 本机管理。

固定 LAN telnet 的完整权限需要另行保存 `TelnetCfg.InitSecLvl=2`，
并在重启后验证诊断命令和配置读写。保持 `Wan_Enable=0`。
临时维护账号权限不能证明固定账号权限；不要在仓库记录真实凭据。

## 源码测试

合并到 `cdf015f` 所在上游基线时执行以下选择器：

```sh
cargo test --offline --locked --lib --no-default-features --features all-devices,acs,mqtt,npu devices::
cargo test --offline --locked --lib --no-default-features devices::
cargo test --offline --locked --test capabilities --no-default-features --features all-devices,acs,mqtt,npu
```

覆盖型号分发、已知表识别、幂等处理、未知表拒绝及 feature 关闭路径。
这些测试不代表已验证 CWMP 参数同步。

| 验证 | 结果 |
| --- | --- |
| 全设备 feature 的 `devices::` 测试 | 107 通过，1 忽略 |
| 无默认 feature 的 `devices::` 测试 | 22 通过，2 忽略 |
| 全设备 feature 的 capabilities 集成测试 | 1 通过 |
| `cargo check --all-targets`，无默认 feature，启用 all-devices,acs,mqtt,npu | 通过 |

## 实机测试清单

现场验证日期：2026-11-05。主猫使用 PonWrt
`r42600-03c1345447`；实机结果仅适用于上述 F650A 固件。

| 项目 | 结果 | 证据与限制 |
| --- | --- | --- |
| 光侧注册 | 通过 | O5；重启后重新注册 |
| EVTO 下发 | 通过 | 三条透明规则完整回读 |
| ARP 入桥 | 通过 | 子猫 nbif0 捕获正确、无业务 VLAN 的目标 ARP |
| LAN 双向转发 | 条件性通过 | 关闭厂商 ARP 接管后，两个方向小包及 1400 字节载荷各 10/10，零丢包 |
| ARP 修复持久性 | 通过 | 两个 LAND 行设为 0，重启后全局运行值仍为 0；再次双向测试通过 |
| 固定 LAN telnet 权限 | 通过 | 重启后固定账号认证、brctl、ebtables、DB 读写均可用 |
| PON 到本机管理地址 | 未通过 | 主猫到子猫 LAN 管理地址 ping 3/3 丢失；桥接成功不代表本机管理可达 |
| CWMP 管理能力 | 未验证 | 有 ME 340 和 tr069d，仅证明存在入口，未验证 Inform/管理通道 |
| Wi-Fi/WAN/绑定同步 | 未验证 | 未做参数写入验证 |
| DHCP、上网、吞吐 | 未验证 | ICMP 结果不能替代这些测试 |
| 本机回退 | 通过 | 恢复两个 LAND 行为 1，保存并重启验证；固定 LAN telnet 权限保留 |

## 最终现场状态

用户决定换用专用 FTTR 子猫，已要求结束 F650A 排查。厂商 ARP 接管
恢复原值 `1`，因此上述条件性转发成功是历史测试结果，不是当前状态承诺。
固定 LAN telnet 完整权限保留，WAN telnet 关闭。
光侧 OMCI 配置按用户最后确认保留；未恢复出厂，也未修改 Wi-Fi 或运营商账号。

## 下一步清单（暂停，须重新授权）

完整接手环境、源码恢复和现场备份见
[跨机器交接](handoff-20261105.md)。

- [ ] 用户明确重新启用前保持当前回退状态，不主动重新关闭 ARP 接管。
- [ ] 新接入时核对精确型号/固件、LAN telnet 权限和 WAN telnet 关闭；
  读取两个 LAND 行及运行 `arp_stolen`，不能假设设备未被后续改动。
- [ ] 管理方向优先发现本机管理 WAN、ME 340、tr069d、ACS 参数及隔离/
  路由条件；先解决 PON 到本机管理，不能把历史 LAN 桥成功当作替代。
- [ ] 不沿用 TEWA 的 D-Bus 参数路径；真实 Inform 后才做身份和类型合同。
- [ ] 如重试 LAN 转发，重新确认全局开关两行的影响及重启授权；
  未知 EVTO 表仍拒绝覆盖，完整配置备份不能直接覆盖保留的 telnet 设置。
- [ ] 分别验证 DHCP、双向业务、吞吐、TR-069、目标同步字段和重启保持；
  当前这些能力未验证的结论不因历史 ICMP 成功而改变。
