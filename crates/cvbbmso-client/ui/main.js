const { core, event, window: tauriWindow } = window.__TAURI__;
const exitConfirmation = document.querySelector("#exit-confirmation");
const confirmExit = document.querySelector("#confirm-exit");
const exitError = document.querySelector("#exit-error");
function renderExitConfirmation(view) {
  const pending = view !== null && view.state.execution !== "finished";
  confirmExit.disabled = pending;
  document.querySelector("#exit-description").textContent = ["finalizing", "finished"].includes(view?.state.execution)
    ? "考试结束后，请等待监考老师指示后再关闭客户端。"
    : "考试期间考生请勿擅自退出监考程序，如有问题请联系监考老师。";
  exitError.textContent = view?.state.execution === "finalizing"
    ? "正在保存录像或等待监考服务确认。"
    : "当前考试仍在进行，请联系监考老师。";
  exitError.hidden = !pending;
}

async function refreshExitConfirmation() {
  confirmExit.disabled = true;
  try {
    renderExitConfirmation(await core.invoke("get_exam_status"));
  } catch {
    exitError.textContent = "无法确认数据上报状态，请稍后重试。";
    exitError.hidden = false;
  }
}

await event.listen("confirm-exit", async () => {
  if (!exitConfirmation.open) {
    exitConfirmation.showModal();
  }
  await refreshExitConfirmation();
});
confirmExit.addEventListener("click", async () => {
  confirmExit.disabled = true;
  try {
    await core.invoke("confirm_exit");
    exitConfirmation.close();
  } catch (error) {
    await refreshExitConfirmation();
    if (error !== "reporting_pending") {
      exitError.textContent = "无法退出客户端，请重试。";
      exitError.hidden = false;
    }
  }
});
const preflight = document.querySelector("#vm-preflight");
const vmStatus = document.querySelector("#vm-status");
const vmDiagnostic = document.querySelector("#vm-diagnostic");
const vmRetry = document.querySelector("#vm-retry");
const studentControls = document.querySelector("#student-controls");
const admissionPanel = document.querySelector("#admission-panel");
const admissionStatus = document.querySelector("#admission-status");
const admissionRetry = document.querySelector("#admission-retry");
const admissionAuthorize = document.querySelector("#admission-authorize");
const admissionExit = document.querySelector("#admission-exit");
const admittedControls = document.querySelector("#admitted-controls");
let admissionRevision = 0;
let authorizationRequested = false;

const admissionErrors = new Map([
  ["elevation_required", "请以管理员身份启动客户端。"],
  ["macos_unsupported", "CVBBMSO 需要 macOS 14 或更新版本，请联系考试管理员。"],
  ["macos_untrusted_installation", "请将监考方提供的 CVBBMSO.app 拖入「应用程序」，再从那里打开。"],
  ["macos_service_unavailable", "未能连接考试网络守卫。请退出客户端，再从「应用程序」打开 CVBBMSO。"],
  ["macos_network_restart_required", "网络预检未通过。请检查网络连接和代理设置，退出客户端后从「应用程序」重新打开 CVBBMSO。"],
  ["macos_authorization_required", "需要管理员授权才能部署考试网络守卫。"],
  ["macos_authorization_cancelled", "管理员授权已取消，可以重试。"],
  ["macos_authorization_failed", "无法部署或启动考试网络守卫，请重试或联系监考管理员。"],
  ["macos_unsupported_pf_configuration", "当前 PF 配置无法启用考试网络限制，请联系考试管理员后重试。"],
  ["macos_busy", "另一个客户端或守卫会话仍在运行，请关闭后重试。"],
  ["monitor_unconfigured", "未配置监考服务地址，请联系考试管理员。"],
  ["inspection_failed", "无法完成网络预检，请重试。"],
  ["proxy_configured", "检测到系统代理，请关闭代理后重试。"],
  ["proxy_unknown", "无法确认系统代理状态，请联系考试管理员。"],
  ["tunnel_configured", "检测到 TUN 或 VPN 接管系统流量，请关闭后重试。"],
  ["tunnel_unknown", "无法确认 TUN 或 VPN 路由状态，请联系考试管理员。"],
  ["network_check_timeout", "网络环境检查超时，请保持客户端运行。"],
  ["monitor_unreachable", "无法连接监考服务，请检查网络后重试。"],
  ["guard_install_failed", "无法启用出站网络守卫，请联系考试管理员。"],
  ["guard_lost", "出站网络守卫校验失败，客户端将结束当前考试。请联系监考人员后重新启动客户端。"],
  ["screen_permission", "请在系统设置中允许客户端录制屏幕，再重试。"],
  ["screen_restart_required", "屏幕录制权限已设置。请退出客户端，再从「应用程序」重新打开 CVBBMSO。"],
]);
const runtimeNetworkErrors = new Map([
  ["proxy_configured", "检测到系统代理，请关闭代理。"],
  ["tunnel_configured", "检测到 TUN 或 VPN 接管系统流量，请关闭该模式。"],
]);
const restartRequired = new Set([
  "elevation_required", "monitor_unconfigured", "guard_lost", "screen_restart_required",
  "macos_unsupported", "macos_untrusted_installation", "macos_service_unavailable", "macos_network_restart_required",
]);
const authorizationReasons = new Set([
  "macos_authorization_required", "macos_authorization_cancelled", "macos_authorization_failed",
]);

async function authorizeEgress() {
  admissionAuthorize.disabled = true;
  try {
    await core.invoke("authorize_macos_egress");
  } catch {
    if (!admissionAuthorize.hidden) {
      admissionStatus.textContent = "无法请求管理员授权，请重试。";
    }
  } finally {
    admissionAuthorize.disabled = false;
  }
}

async function refreshAdmission(retry = false) {
  const revision = ++admissionRevision;
  try {
    const result = await core.invoke("admission_status", { retry });
    if (revision !== admissionRevision) return;
    const ready = result.status === "ready";
    admittedControls.hidden = !ready;
    admissionPanel.hidden = ready;
    const needsAuthorization = result.status === "blocked" && authorizationReasons.has(result.reason);
    admissionAuthorize.hidden = !needsAuthorization;
    admissionRetry.hidden = result.status !== "blocked" || needsAuthorization || restartRequired.has(result.reason);
    const cleanupFailed = result.status === "cleanup_failed";
    admissionExit.hidden = !cleanupFailed
      && !(result.status === "blocked" && result.reason === "macos_network_restart_required");
    if (cleanupFailed) {
      preflight.hidden = true;
      studentControls.hidden = false;
      const diagnostic = result.reason.diagnostic_saved
        ? "本地诊断已保存到应用数据目录中的 egress-cleanup.json。"
        : "无法保存本地诊断，请记录错误代码。";
      admissionStatus.textContent = `网络守卫清理失败（${result.reason.code}）。请联系监考人员。${diagnostic}`;
      return;
    }
    if (result.status === "exiting") {
      admissionStatus.textContent = "正在关闭客户端…";
    } else if (result.status === "authorizing") {
      admissionStatus.textContent = "正在等待管理员授权并启动考试网络守卫。请完成或取消系统授权后再退出。";
    } else {
      admissionStatus.textContent = admissionErrors.get(result.reason) ?? "正在检查考试环境…";
    }
    if (result.status === "restricted") {
      admissionStatus.textContent = (runtimeNetworkErrors.get(result.reason) ?? admissionStatus.textContent)
        + " 网络环境恢复后，将自动重新开放入场。进行中的考试仍会录像。";
    }
    if (result.reason === "macos_authorization_required" && !authorizationRequested) {
      authorizationRequested = true;
      void authorizeEgress();
    }
  } catch {
    if (revision !== admissionRevision) return;
    admissionPanel.hidden = false;
    admittedControls.hidden = true;
    admissionAuthorize.hidden = true;
    admissionStatus.textContent = "无法读取考试环境检查结果。";
  }
}

admissionRetry.addEventListener("click", () => refreshAdmission(true));
admissionAuthorize.addEventListener("click", authorizeEgress);
admissionExit.addEventListener("click", () => {
  tauriWindow.getCurrentWindow().close().catch(() => {
    admissionStatus.append(" 请关闭客户端窗口以退出。");
  });
});
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
    vmStatus.textContent = result.reason === "placeholder_identity"
      ? "设备固件提供的厂商或型号无法核验。请联系考试管理员。"
      : "无法完成设备检查，请重试。";
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
const examFeedback = document.querySelector("#exam-session-feedback");
const currentExam = document.querySelector("#current-exam");
const examAlert = document.querySelector("#exam-alert");
const currentExamTitle = document.querySelector("#current-exam-title");
const currentExamStatus = document.querySelector("#current-exam-status");
const closeExam = document.querySelector("#close-exam");
const examListRefreshMs = 30_000;
let exams = [];
let examListTimer;
let examView = null;
let examSelection = null;
let examBusy = false;
let examEventsReady = false;
let statusRevision = 0;

const examErrors = new Map([
  ["preflight_blocked", "设备检查通过后才能继续。"],
  ["sign_in_required", "请在 Accoding 窗口登录或重新登录，再刷新列表。"],
  ["session_unreadable", "无法读取 Accoding 会话，请重新登录后刷新。"],
  ["monitor_unconfigured", "未配置监考服务地址，请联系考试管理员。"],
  ["connection_failed", "无法连接监考服务，请检查网络后重试。"],
  ["service_unavailable", "监考服务暂时无法核验身份，请稍后重试。"],
  ["unexpected_response", "监考服务返回了异常响应，请稍后重试。"],
]);
const examSessionErrors = new Map([
  ["admission_unavailable", "考试环境检查尚未通过，请查看检查结果。"],
  ["session_busy", "已有考试会话，请先退出当前考试。"],
  ["session_closed", "考试会话已关闭，请重新选择。"],
  ["not_enrolled", "监考服务未核验到本场考试的报名。"],
  ["exam_ended", "考试已结束，当前账号没有待确认的收尾记录。"],
  ["storage_failed", "无法读取或保存本地考试数据。"],
  ["recording_failed", "无法初始化本场考试的录像数据。"],
  ["shutdown_failed", "考试会话已结束，但本地收尾失败，请联系考试管理员。"],
  ["reporting_pending", "请在监考服务确认收尾后再退出当前考试。"],
]);
const executionStates = new Map([
  ["ready", "等待开始"],
  ["recording", "考试中"],
  ["finalizing", "正在收尾"],
]);
const recordingStates = new Map([
  ["idle", "待命"],
  ["capturing", "录制中"],
  ["finalizing", "正在收尾"],
  ["complete", "已完成"],
  ["failed", "失败"],
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

function renderExamEntry(button) {
  const entered = button.dataset.examId === examView?.exam_id;
  button.textContent = entered ? "已进入" : "进入考试";
  button.disabled = !examEventsReady || examBusy || examView !== null;
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
  const enter = document.createElement("button");
  enter.type = "button";
  enter.className = "open-exam";
  enter.dataset.examId = exam.id;
  renderExamEntry(enter);
  enter.addEventListener("click", () => changeExamSession(exam));
  item.append(title, phase, times, enter);
  return item;
}

function canOpenExam(exam) {
  return exam.phase !== "ended";
}

function renderExamList() {
  const rows = document.createDocumentFragment();
  let count = 0;
  for (const exam of exams) {
    if (!canOpenExam(exam)) continue;
    rows.append(renderExam(exam));
    count++;
  }
  examList.replaceChildren(rows);
  examStatus.textContent = count > 0 ? `已加载 ${count} 场考试。`
    : exams.length > 0 ? "暂无可参加的考试。" : "已登录，暂无报名考试。";
}

function renderExamSession() {
  for (const button of examList.querySelectorAll(".open-exam")) {
    renderExamEntry(button);
  }
  closeExam.disabled = examBusy;
  if (exitConfirmation.open) renderExitConfirmation(examView);
  currentExam.hidden = examView === null;
  examAlert.hidden = true;
  const needsSignIn = examView?.state.execution !== "finished"
    && examView?.last_error_code === "ACCODING_AUTH_EXPIRED";
  open.textContent = needsSignIn ? "重新登录 Accoding" : "打开 Accoding";
  if (!examView) {
    currentExamTitle.textContent = "";
    currentExamStatus.textContent = "";
    return;
  }
  const { exam_id, connected, state, last_error_code } = examView;
  const finished = state.execution === "finished";
  if (finished && examFeedback.textContent === examSessionErrors.get("reporting_pending")) {
    examFeedback.textContent = "";
  }
  const alerts = [];
  if (!connected && state.execution !== "finished") {
    alerts.push("监考连接已断开，正在自动重连。请保持客户端运行。");
  }
  if (state.recording === "failed") {
    alerts.push(state.execution === "recording"
      ? "录屏暂时中断，正在尝试恢复。请保持客户端运行。"
      : "录像收尾异常，请保持客户端运行并联系监考老师。");
  }
  examAlert.textContent = alerts.join(" ");
  examAlert.hidden = alerts.length === 0;
  const exam = examSelection?.id === exam_id ? examSelection : exams.find(({ id }) => id === exam_id);
  currentExamTitle.textContent = exam?.title ?? exam_id;
  const status = finished
    ? ["考试已结束", state.recording === "complete" ? "录像已保存" : "录像收尾失败", "请等待监考老师指示后再关闭客户端。"]
    : [executionStates.get(state.execution), `录像${recordingStates.get(state.recording)}`,
      connected ? "已连接监考服务" : "正在重连监考服务"];
  if (state.execution === "finalizing") status.push("考试时间已结束，正在等待监考服务确认收尾。");
  if (needsSignIn) status.push("请用本场考试账号重新登录 Accoding。");
  else if (last_error_code) {
    status.push(`错误 ${last_error_code}`);
    if (!connected && last_error_code === "INTERNAL_ERROR") {
      status.push("可点击「检查会话」重试登录验证。");
    }
  }
  currentExamStatus.textContent = status.join(" · ");
}

async function refreshExamSession() {
  if (examBusy) return;
  const revision = ++statusRevision;
  try {
    const view = await core.invoke("get_exam_status");
    if (revision === statusRevision) {
      examView = view;
      renderExamSession();
    }
  } catch {
    if (revision === statusRevision && !examFeedback.textContent) {
      examFeedback.textContent = "无法读取当前考试状态。";
    }
  }
}

async function changeExamSession(exam) {
  if (examBusy || (exam && examView) || (!exam && !examView)) return;
  if (exam && !canOpenExam(exam)) {
    renderExamList();
    examFeedback.textContent = "这场考试已结束。";
    return;
  }
  examBusy = true;
  ++statusRevision;
  renderExamSession();
  examFeedback.textContent = exam ? "正在连接考试…" : "正在退出考试…";
  try {
    if (exam) {
      examSelection = exam;
      await core.invoke("open_exam", { exam_id: exam.id });
    } else {
      await core.invoke("close_exam");
      examView = null;
      examSelection = null;
    }
    examFeedback.textContent = "";
  } catch (error) {
    if (exam && error === "exam_ended") {
      exams = exams.filter(({ id }) => id !== exam.id);
      renderExamList();
    }
    examFeedback.textContent = examSessionErrors.get(error) ?? examErrors.get(error) ?? "考试操作失败，请重试。";
  } finally {
    examBusy = false;
    await refreshExamSession();
    renderExamSession();
  }
}

closeExam.addEventListener("click", () => changeExamSession(null));

async function refreshExamList(manual) {
  if (refreshExams.disabled) return;
  refreshExams.disabled = true;
  clearTimeout(examListTimer);
  if (manual) {
    exams = [];
    examList.replaceChildren();
    examList.setAttribute("aria-busy", "true");
    examStatus.textContent = "正在加载报名列表…";
  }
  try {
    exams = await core.invoke("list_exams");
    renderExamList();
  } catch (error) {
    if (manual) examStatus.textContent = examErrors.get(error) ?? "获取报名列表失败，请重试。";
  } finally {
    examList.removeAttribute("aria-busy");
    refreshExams.textContent = "刷新列表";
    refreshExams.disabled = false;
    if (exams.some(canOpenExam)) {
      examListTimer = setTimeout(() => refreshExamList(false), examListRefreshMs);
    }
  }
}
refreshExams.addEventListener("click", () => refreshExamList(true));
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

try {
  // Treat events as notifications. A WebView may deliver an already queued
  // event after closing or reopening the same exam.
  await event.listen("cvbbmso-status", refreshExamSession);
  examEventsReady = true;
  await refreshExamSession();
} catch {
  examFeedback.textContent = "考试状态通知暂不可用。";
}
renderExamSession();
// The native snapshot remains authoritative when a WebView event goes undelivered.
setInterval(() => {
  if (examView !== null && examView.state.execution !== "finished") void refreshExamSession();
}, 3000);

try {
  await event.listen("admission-status", () => refreshAdmission());
} finally {
  await refreshAdmission();
}
