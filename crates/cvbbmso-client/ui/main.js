const { core, event } = window.__TAURI__;
const session = document.querySelector("#session");
const open = document.querySelector("#open");
const inspect = document.querySelector("#inspect");

async function submit(button, command) {
  button.disabled = true;
  try {
    await core.invoke(command);
  } catch (error) {
    session.textContent = typeof error === "string" ? error : "操作失败，请重试。";
  } finally {
    button.disabled = false;
  }
}

open.addEventListener("click", () => submit(open, "open_accoding"));

try {
  await event.listen("accoding-session", ({ payload }) => {
    session.textContent = payload === "cookie_available"
      ? "已读取会话，登录状态待监考服务核验。"
      : "未读取到会话。请在 Accoding 窗口登录后重试。";
  });
  inspect.addEventListener("click", () => submit(inspect, "inspect_session"));
  inspect.disabled = false;
} catch {
  session.textContent = "会话检查暂不可用。";
}
