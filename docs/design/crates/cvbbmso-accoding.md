# cvbbmso-accoding — Accoding Cookie 身份适配

本 crate 负责 Accoding 学生站 Cookie 的规范化，以及服务端对学生身份的复验。客户端通过受控 WebView 完成原生登录，服务器用 Cookie 请求 `https://accoding.buaa.edu.cn/api/users/me`，获得登录用户的 `id`、`username`、`nickname`。

## 公开 Rust API

```rust
pub struct AccodingCookies { /* 私有 Cookie 集合 */ }
pub struct Verifier { /* 私有 HTTP 客户端 */ }

#[non_exhaustive]
pub enum AccodingError {
    MissingSession,
    InvalidHeader(Option<http::header::InvalidHeaderValue>),
    Unauthenticated,
    Upstream(reqwest::Error),
    InvalidResponse(Option<serde_json::Error>),
}

impl AccodingCookies {
    /// 从 WebView Cookie Store 或请求头解析结果构造会话 Cookie。
    /// 要求 connect.sid 存在且非空；其余 Cookie 原样保留。
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, AccodingError>;

    /// 返回构造时已校验并标为敏感的 HTTP Cookie 头，供 HTTPS/WSS 请求使用。
    pub fn header_value(&self) -> http::HeaderValue;
}

impl Verifier {
    /// 创建可复用的学生站身份验证器。
    pub fn new() -> Result<Self, AccodingError>;

    /// 查询当前 Accoding 账号；匿名或失效 Cookie 返回 Unauthenticated。
    pub async fn verify(
        &self,
        cookies: &AccodingCookies,
    ) -> Result<cvbbmso_protocol::AccodingUser, AccodingError>;
}
```

`AccodingCookies` 构造时要求恰好有一个非空 `connect.sid`。缺失或空值返回 `MissingSession`，重复的会话 Cookie 或不符合 Cookie 头语法的名值返回 `InvalidHeader`。Cookie 值和顺序原样保留，服务端用整组 Cookie 复验。

`Verifier` 只向固定的学生站身份接口发送 Cookie。HTTP 客户端的请求超时为 10 秒，并禁用重定向。`401`、指向同 Origin `/user/login` 的 `301`、`302`、`303`、`307` 或 `308`，以及 `200` 的空正文或纯 ASCII 空白正文，均返回 `Unauthenticated`。

身份响应正文最多接收 16 KiB（16,384 字节）。`Verifier` 按实际读取的字节累计，超出上限时立即停止读取并返回 `InvalidResponse`。异常响应状态、无效 JSON 和不满足 `AccodingUser` 约束的身份数据也返回 `InvalidResponse`。HTTP 客户端初始化失败、传输错误和其他上游 HTTP 错误状态返回 `Upstream`。

服务端对 HTTP `/api/exams` 和 WebSocket 升级前的握手使用同一错误映射。非法 Cookie 或重复的 `connect.sid` 产生 `InvalidHeader`，返回 `400 INVALID_REQUEST`。`MissingSession` 和 `Unauthenticated` 返回 `401 ACCODING_AUTH_EXPIRED`。`Upstream` 和 `InvalidResponse` 映射为 `ACCODING_UNAVAILABLE` 或 `INTERNAL_ERROR`。

## Accoding 登录与认证协议

| 操作 | 学生站 | 方法及参数 |
| --- | --- | --- |
| 获取登录页 | `https://accoding.buaa.edu.cn/user/login` | `GET`，获得表单和 Cookie |
| 提交登录 | `https://accoding.buaa.edu.cn/user/login` | `POST application/x-www-form-urlencoded`，`username`、`password`、`_csrf` |
| 查询账号 | `https://accoding.buaa.edu.cn/api/users/me` | `GET`，携带最新 Cookie |
| 退出登录 | `https://accoding.buaa.edu.cn/user/logout` | `GET`，携带 Cookie |

`GET /user/login` 返回 HTML 登录表单，并设置会话 Cookie；登录表单中的 `_csrf` 与当前 Cookie 一并提交。登录响应和重定向的 `Set-Cookie` 由 WebView 更新到 Cookie Store。

```http
POST /user/login HTTP/1.1
Host: accoding.buaa.edu.cn
Content-Type: application/x-www-form-urlencoded
Cookie: connect.sid=<value>

username=user%40example.com&password=<url-encoded-password>&_csrf=<url-encoded-csrf>
```

`GET /api/users/me` 的已登录响应为 HTTP `200` 和 JSON 对象：

```json
{"id":12345,"username":"user@example.com","nickname":"示例用户"}
```

匿名响应同样为 HTTP `200`，Content-Type 为 `application/json`，正文为空。

## 与其他 crate 的边界

`cvbbmso-client` 读取原生 Cookie Store 并创建 `AccodingCookies`。`cvbbmso-exam` 在监考服务器 WSS 握手时发送 Cookie。`cvbbmso-server` 调用 `Verifier::verify`，将复验得到的 `id` 和 `username` 交给报名流程。学号匹配依赖 Accoding 正确发放校园账号。本 crate 校验 Accoding 会话，由考务方维护学校报名名单。
