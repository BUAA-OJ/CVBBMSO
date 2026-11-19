# cvbbmso-accoding — Accoding Cookie 身份适配

本 crate 负责 Accoding 学生站 Cookie 的规范化，以及服务端对学生身份的复验。客户端通过受控 WebView 完成原生登录，服务器用 Cookie 请求 `https://accoding.buaa.edu.cn/api/users/me`，获得登录用户的 `id`、`username`、`nickname`。

## 公开 Rust API

```rust
pub struct AccodingCookies { /* 私有 Cookie 集合 */ }

pub enum CookieError {
    MissingSession,
    InvalidHeader,
}

impl AccodingCookies {
    /// 从 WebView Cookie Store 或请求头解析结果构造会话 Cookie。
    /// 要求 connect.sid 存在且非空；其余 Cookie 原样保留。
    pub fn from_pairs(
        pairs: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, CookieError>;

    /// 编码为标准 HTTP Cookie 头，供原生 HTTPS/WSS 请求使用。
    pub fn header_value(&self) -> Result<http::HeaderValue, CookieError>;
}

pub struct Verifier { /* 私有 HTTP 客户端 */ }

pub enum VerifyError {
    Unauthenticated,
    Upstream,
    InvalidResponse,
}

impl Verifier {
    /// 创建学生站身份验证器；学生站 Origin 是内部固定常量。
    pub fn new(http: reqwest::Client) -> Self;

    /// 查询当前 Accoding 账号；匿名或失效 Cookie 返回 Unauthenticated。
    pub async fn verify(
        &self,
        cookies: &AccodingCookies,
    ) -> Result<cvbbmso_protocol::AccodingUser, VerifyError>;
}
```

`AccodingCookies` 要求至少包含非空 `connect.sid`；WebView 提供的其他 Cookie 原样保留，服务端用整组 Cookie 复验。`Verifier` 的 Origin 固定为学生站，Cookie 发送到该固定地址。

`VerifyError::Unauthenticated` 表示 Cookie 缺失、失效或 Accoding 返回匿名状态；`Upstream` 表示网络错误或学生站 5xx；`InvalidResponse` 表示返回体格式异常。服务端将 `Unauthenticated` 映射为 `ACCODING_AUTH_EXPIRED`，将 `Upstream` / `InvalidResponse` 映射为 `ACCODING_UNAVAILABLE` 或 `INTERNAL_ERROR`。

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

匿名响应同样为 HTTP `200`，Content-Type 为 `application/json`，正文为空。`Verifier::verify` 在得到有效用户对象时返回 `AccodingUser`；空正文映射为 `Unauthenticated`，监考服务端在握手认证失败时映射为 `401 ACCODING_AUTH_EXPIRED`。

## 与其他 crate 的边界

`cvbbmso-client` 读取原生 Cookie Store 并创建 `AccodingCookies`；`cvbbmso-exam` 在监考服务器 WSS 握手时发送 Cookie；`cvbbmso-server` 从 HTTP 请求头构造 `AccodingCookies`，调用 `Verifier::verify`，将 `AccodingUser.id` 交给 `cvbbmso-domain`。本 crate 的职责限于 Cookie 和身份复验。
