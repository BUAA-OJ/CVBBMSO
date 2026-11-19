const { core, event, window: tauriWindow } = window.__TAURI__;
const preflight = document.querySelector("#vm-preflight");
const vmStatus = document.querySelector("#vm-status");
const vmDiagnostic = document.querySelector("#vm-diagnostic");
const vmRetry = document.querySelector("#vm-retry");
const studentControls = document.querySelector("#student-controls");

async function checkDevice(retry) {
  vmRetry.hidden = true;
  vmDiagnostic.hidden = true;
  vmStatus.textContent = "正在检查运行环境…";
  const result = await core.invoke("vm_preflight", { retry }).catch(() => ({ status: "unknown" }));
  if (result.status === "not_detected") {
    preflight.hidden = true;
    studentControls.hidden = false;
  } else if (result.status === "detected") {
    vmStatus.textContent = "检测到虚拟机。请在实体设备上运行客户端。";
  } else {
    vmStatus.textContent = "无法完成设备检查，请重试。";
    if (result.reason) {
      vmDiagnostic.textContent = `诊断信息：${result.reason}`;
      vmDiagnostic.hidden = false;
    }
    vmRetry.hidden = false;
  }
}

vmRetry.addEventListener("click", () => checkDevice(true));
document.querySelector("#vm-quit").addEventListener("click", () => {
  tauriWindow.getCurrentWindow().close().catch(() => {
    vmStatus.textContent = "请关闭客户端窗口以退出。";
  });
});

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

const refreshExams = document.querySelector("#refresh-exams");
const examStatus = document.querySelector("#exam-status");
const examList = document.querySelector("#exams");
const examErrors = new Map([
  ["preflight_blocked", "设备检查通过后才能继续。"],
  ["sign_in_required", "请在 Accoding 窗口登录或重新登录，再刷新列表。"],
  ["session_unreadable", "无法读取 Accoding 会话，请重新登录后刷新。"],
  ["monitor_unconfigured", "未配置监考服务地址，请联系考试管理员。"],
  ["connection_failed", "无法连接监考服务，请检查网络后重试。"],
  ["service_unavailable", "监考服务暂时无法核验身份，请稍后重试。"],
  ["unexpected_response", "监考服务返回了异常响应，请稍后重试。"],
]);
const examPhases = new Map([
  ["scheduled", "未开始"],
  ["running", "进行中"],
  ["ended", "已结束"],
]);
const examTime = new Intl.DateTimeFormat("zh-CN", {
  year: "numeric",
  month: "numeric",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  timeZoneName: "short",
});

function renderTime(timestamp) {
  const time = document.createElement("time");
  time.dateTime = timestamp;
  time.textContent = examTime.format(new Date(timestamp));
  return time;
}

function renderExam(exam) {
  const item = document.createElement("li");
  const title = document.createElement("h3");
  title.textContent = exam.title;
  const phase = document.createElement("span");
  phase.className = "exam-phase";
  phase.textContent = examPhases.get(exam.phase);
  const times = document.createElement("p");
  times.append(renderTime(exam.start_at), " 至 ", renderTime(exam.end_at));
  item.append(title, phase, times);
  return item;
}

refreshExams.addEventListener("click", async () => {
  refreshExams.disabled = true;
  examList.replaceChildren();
  examList.setAttribute("aria-busy", "true");
  examStatus.textContent = "正在加载报名列表…";
  try {
    const items = await core.invoke("list_exams");
    const rows = document.createDocumentFragment();
    for (const exam of items) {
      rows.append(renderExam(exam));
    }
    examList.replaceChildren(rows);
    examStatus.textContent = items.length === 0
      ? "已登录，暂无报名考试。"
      : `已加载 ${items.length} 场考试。`;
  } catch (error) {
    examStatus.textContent = examErrors.get(error) ?? "获取报名列表失败，请重试。";
  } finally {
    examList.removeAttribute("aria-busy");
    refreshExams.textContent = "刷新列表";
    refreshExams.disabled = false;
  }
});
refreshExams.disabled = false;
void checkDevice(false);

try {
  await event.listen("accoding-session", ({ payload }) => {
    session.textContent = payload === "cookie_available"
      ? "已读取 Accoding 会话。"
      : "未读取到会话。请在 Accoding 窗口登录后重试。";
  });
  inspect.addEventListener("click", () => submit(inspect, "inspect_session"));
  inspect.disabled = false;
} catch {
  session.textContent = "会话检查暂不可用。";
}
