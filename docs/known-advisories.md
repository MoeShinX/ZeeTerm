# 已知的依赖安全公告

`cargo audit` 报告里目前**有意不处理**的条目。每条写明为什么不处理、什么时候重新看。

## RUSTSEC-2023-0071：`rsa` 的 Marvin 攻击（计时侧信道）

- 影响的 crate：`rsa 0.10.0-rc.x`，经 `russh` / `ssh-key` 引入（根工作区和 `native/` 都有）。
- 状态：上游**没有修复版本**（公告写「No fixed upgrade is available」）。
- 为什么暂不处理：ZeeTerm 只在**客户端**用它，对 RSA 私钥做登录签名、对 RSA 主机密钥验签，不是接收别人密文的服务端解密场景，也没有把解密结果的对错暴露给第三方；公告描述的利用方式（反复观察解密 / 签名计时来恢复密钥）在这个用法下条件很苛刻。已知、记录在案，不是遗漏。
- 重新评估：`rsa` 或 `russh` / `ssh-key` 发布了带修复的版本时升级；ed25519 / ECDSA 密钥不走这个 crate，不受影响，新建密钥建议用 ed25519。

## 放行清单在哪

CI 的「依赖审计」任务（`.github/workflows/ci.yml` 里的 `audit`）用 `cargo audit --ignore <编号>` 放行上面登记的公告，**这份放行清单就是 ci.yml 里那几个 `--ignore`**，和本文件里的 `## RUSTSEC-…` 标题一一对应（`native/zssh-gpui/src/installer_manifest.rs` 里有测试守着，两边不一致会失败）。新增一条登记，就在 ci.yml 里加一个 `--ignore`；删掉一条登记（上游修复、升级后不再命中），同时删掉对应的 `--ignore`。

这个任务只报告、不阻断：失败不影响整次 CI，所以发版不受影响；它报出新公告时，看任务日志，决定升级依赖，或者像上面这样登记一条说明再放行。`cargo audit` 打印的 unmaintained / yanked 之类警告不让它失败，也不放行、不登记（只是提醒）。
