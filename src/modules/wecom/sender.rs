//! wecom 域的**唯一**出网点：企业微信群机器人 webhook 的裸 HTTP POST
//!
//! ## ⚠️ 本域唯一调 `reqwest` 打 webhook 的文件
//!
//! 内容域 `wecom_daily`（每日总结推送）**调本文件的 `send_markdown`**，**不自己再写
//! 一份**。理由同 `mail_digest` 那对（`mailer.rs` 调 lettre、`service.rs` 编排）：
//! 「怎么发」与「发什么」必须分开，否则 errcode 解析、超时、脱敏这三处纪律会被复制
//! 两遍、然后各自漂移。
//!
//! ## ⚠️ 与 `src/modules/wx/wecom_client.rs` **同名不同物**
//!
//! 那个是小程序**登录**客户端（`gettoken` / `jscode2session`，需 corpid / corpsecret，
//! 还带 Redis token 缓存）。本文件的群机器人 webhook 是**裸 HTTP POST、不需任何
//! 凭据**：地址本身（`?key=<uuid>`）就是全部鉴权。两者除域名相同外零交集。
//!
//! ## 官方契约（群机器人 webhook）
//!
//! ```text
//! POST https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=<uuid>
//! Content-Type: application/json
//!
//! → { "errcode": 0,  "errmsg": "ok" }
//! → { "errcode": 93000, "errmsg": "invalid webhook url" }
//! ```
//!
//! ## ★ 头号硬约束：**必须解析响应体的 `errcode`**
//!
//! 企业微信 webhook 的失败**绝大多数是 HTTP 200 + `errcode != 0`**（地址失效、群里
//! 机器人被移除、消息被限频…）。不解析 `errcode` 就是「HTTP 成功 = 推送成功」的**静默
//! 失败** —— 流水里会记一条 SUCCESS、页面上「每日总结照常到达」，而群里什么都没有。
//! 这比任何显式报错都难查，所以本文件把 `classify` 抽成**纯函数**并由 lib 单测逐条
//! 钉住（不发真实网络请求）。
//!
//! ## 安全纪律（三条，逐条都有反例）
//!
//! 1. **URL 永远不进任何日志或错误文案**：`reqwest::Error` 的 `Display` 在
//!    `inner.url` 存在时会追加 ` for url (<完整 URL，含 query>)`，而 query 里就是
//!    `key=`。故打日志前一律先 `.without_url()`（与 `wx::wecom_client.rs` 同款）。
//! 2. **HTTP 非 2xx 时只记 status code，不记 body 全文**：上游在某些错误路径下会把
//!    请求里的东西回显出来。
//! 3. **响应体解析失败只记「无法解析」，不记原文**：同上，且响应体没有诊断价值。
//!
//! ## ⚠️ 官方限频：每个消息推送 ≤ 20 条/分钟
//!
//! 登记在这里以便内容域对齐：**一次触发最多发 `max_chunks` 条**
//! （`t_wecom_daily_config.max_chunks`，范围 `1..=20`），这条配置项就是这条限频的上界。
//! 本文件**不做**限流 / 排队 —— 它是单次出网动作的封装，节流是调度侧（cron job）的职责。
//!
//! ## ⚠️ 内容长度上限：**本文件不做截断、不做分片**
//!
//! | 消息类型 | 上限（官方文档，UTF-8 字节） |
//! |---|---|
//! | `markdown`（v1） | 4096 字节 |
//! | `text` | 2048 字节 |
//!
//! **调用方有责任保证不超限**；分片是内容域（`wecom_daily`）的职责 —— 它按
//! `max_chunks` 切片，本文件只负责把**给定的一段**原样发出去。超限的表现是企业微信
//! 返 `errcode != 0`（`Failed`），不会静默截断。
//!
//! ## ⚠️ markdown 语法子集（v1）只有这些
//!
//! 标题 1-6 级（`#` 与文字间要有空格）/ `**加粗**` / 链接 / 行内代码（不支持跨行）/
//! `>` 引用 / `<font color="info|comment|warning">`。
//! **不支持**列表、表格、分割线、图片、代码块、斜体。
//! （`markdown_v2` 支持表格与列表但**不支持字体颜色**且客户端需 4.1.36+，本仓选 v1 ——
//! 选型理由归内容域，写在 `wecom_daily` 的 template doc 里。）
//!
//! ## 超时
//!
//! [`HTTP_TIMEOUT_MS`] 是**本轮给的模块常量**：任务书明令本轮**不新增 env**、不改
//! `src/infra/config.rs`。⚠️ 将来接线时把它换成 env `WECOM_HTTP_TIMEOUT_MS`（与
//! `infra::config::WeComConfig::http_timeout_ms` 同名不同物 —— 那是小程序登录那条链路
//! 的超时，两者互不共享）。

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

/// 单次 webhook POST 的结果分类。
///
/// ⚠️ **`Failed` 的文本是给人看的，绝不含 webhook URL 或 key**（见文件头「安全纪律」）。
/// lib 单测逐条断言这一点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Ok,
    /// 人类可读原因。`reqwest` 错误、HTTP 非 2xx、`errcode != 0`、响应体无法解析
    /// 四种成因都落在这里 —— 调用方（内容域）把整段话写进 `t_wecom_daily_log
    /// .error_message` 即可，**不需要**再分类。
    Failed(String),
}

impl SendOutcome {
    /// 是否推送成功。
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// 单次 webhook POST 的超时（毫秒）。
///
/// 官方 webhook 是**同步应答**（发完立刻返 errcode），正常在百毫秒级；给 5s 是为了让
/// 「网络慢」不至于立刻判失败，又不至于让一次推送把 cron job 挂住。⚠️ 待接线，见文件头。
const HTTP_TIMEOUT_MS: u64 = 5_000;

/// 从响应体里截取 `errmsg` 时保留的最大字符数。
///
/// ⚠️ `errmsg` 是**服务端生成**的（不会回显我们的 key），但它会原样进
/// `t_wecom_daily_log.error_message`（`varchar(500)`），所以仍要截断，避免一条超长
/// errmsg 把流水行写爆。
const ERRMSG_MAX_CHARS: usize = 200;

/// 发一条**纯文本**探针（`/webhooks/{id}/test` 用）。
///
/// ⚠️ 探针内容固定为一句自报家门，**不含任何业务数据**：这个端点是「管理员点一下看通
/// 不通」，不是「拿真数据试一发」。
pub async fn send_text(url: &str, content: &str) -> SendOutcome {
    send("text", url, content).await
}

/// 发一条 **markdown** 消息（内容域每日总结用）。
pub async fn send_markdown(url: &str, content: &str) -> SendOutcome {
    send("markdown", url, content).await
}

/// 两个消息类型的唯一实现体。
async fn send(msgtype: &str, url: &str, content: &str) -> SendOutcome {
    // ⚠️ 只接受两种 msgtype —— 本文件**不**接受调用方传入的模板形状，那会把「消息体
    // 长什么样」这个知识散到调用点，将来企微改协议时要改 N 处。
    let payload = match msgtype {
        "text" => json!({"msgtype": "text", "text": {"content": content}}),
        "markdown" => json!({"msgtype": "markdown", "markdown": {"content": content}}),
        other => {
            return SendOutcome::Failed(format!("内部错误：不支持的 webhook 消息类型 {other}"));
        }
    };

    let client = match client() {
        Ok(c) => c,
        Err(msg) => return SendOutcome::Failed(msg),
    };

    let resp = match client.post(url).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
            // ⚠️ `.without_url()` 必须在格式化**之前**：reqwest 的 `Display` 会把完整
            // URL（含 `key=`）拼进错误文案，而 URL 里的 key 就是企业微信的 bearer 凭证。
            // （与 `wx::wecom_client.rs` 的 R1 订正同款纪律。）
            let e = e.without_url();
            tracing::warn!(error = %e, "企业微信 webhook 请求失败（网络层）");
            return SendOutcome::Failed(format!("企业微信 webhook 请求失败：{e}"));
        }
    };

    let status = resp.status().as_u16();
    // ⚠️ 先看 HTTP 层。**不把 body 全文带进文案** —— 某些错误路径会回显请求内容。
    if !resp.status().is_success() {
        return classify(status, "");
    }
    // 读 body 本身也可能失败（连接被对端掐断），同样按「无法解析」处理、不记原文。
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            let e = e.without_url();
            tracing::warn!(error = %e, "企业微信 webhook 响应体读取失败");
            return SendOutcome::Failed(format!("企业微信 webhook 响应体读取失败：{e}"));
        }
    };
    classify(status, &body)
}

/// 把「HTTP 状态码 + 响应体」归类成 [`SendOutcome`] —— **纯函数，不发网络请求**。
///
/// ## 四条分支（lib 单测逐条钉住）
///
/// | 输入 | 结果 |
/// |---|---|
/// | 2xx + `{"errcode":0,…}` | `Ok` |
/// | 2xx + `errcode != 0` | `Failed`（带 `errcode` 与 `errmsg`） |
/// | 2xx + **不是合法 JSON** / 缺 `errcode` | `Failed`（「响应体无法解析」） |
/// | 非 2xx | `Failed`（**只带 status code**） |
///
/// ⚠️ **非 2xx 时 `body` 参数被刻意忽略**：即使调用方顺手把 body 传了进来也不进文案。
pub fn classify(http_status: u16, body: &str) -> SendOutcome {
    if !(200..300).contains(&http_status) {
        return SendOutcome::Failed(format!("企业微信 webhook 返回 HTTP {http_status}"));
    }

    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return SendOutcome::Failed("企业微信 webhook 响应体无法解析（不是合法 JSON）".to_string());
    };
    let Some(errcode) = v.get("errcode").and_then(Value::as_i64) else {
        return SendOutcome::Failed(
            "企业微信 webhook 响应体无法解析（缺 errcode 字段）".to_string(),
        );
    };
    if errcode == 0 {
        return SendOutcome::Ok;
    }

    let errmsg = v
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or("（企业微信未返回 errmsg）");
    let errmsg: String = errmsg
        .chars()
        .filter(|c| !c.is_control())
        .take(ERRMSG_MAX_CHARS)
        .collect();
    SendOutcome::Failed(format!("企业微信返回 errcode={errcode}：{errmsg}"))
}

/// 进程内复用的 `reqwest::Client`。
///
/// ⚠️ **缓存而不是每次新建**：新建一次 Client = 一次全新连接池 + 一次 TLS 握手，而
/// 一次触发最多发 20 条（见文件头的限频段）⇒ 每条重建会让 TLS 握手成本翻十几倍。
/// `OnceLock<Result<Client, String>>` 让「构造失败」也只发生一次（之后每次都拿到同一个
/// 错误字符串），不会每次都重试构造。
fn client() -> Result<reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(build_client).clone()
}

fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(HTTP_TIMEOUT_MS))
        // 不跟随重定向：webhook 是固定地址，跟随重定向只会把带 key 的 URL 送到别处。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // ⚠️ 构造失败的文案里也不带 URL（此刻根本没有 URL）。
        .map_err(|e| format!("构造企业微信 webhook HTTP 客户端失败：{e}"))
}
