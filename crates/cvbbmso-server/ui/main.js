"use strict";

const element = (id) => document.getElementById(id);
const formatTime = (value) => value ? new Date(value).toLocaleString("zh-CN") : "—";
const phases = { scheduled: "未开始", running: "进行中", ended: "已结束" };
const stages = { ready: "就绪", recording: "录像中", finalizing: "收尾中", finished: "已结束" };
const errorMessages = {
  ADMIN_AUTH_REQUIRED: "管理员令牌无效或已轮换，请重新登录。",
  INVALID_INPUT: "输入无效，请检查考试时间、录像参数或学号。",
  NOT_FOUND: "未找到对应记录。",
  CONFLICT: "考试状态已变化，当前操作无法执行。",
  STORAGE_BUSY: "服务端正在处理其他写入，请稍后重试。",
  BODY_TOO_LARGE: "导入内容过大，请分批操作。",
};
let session = null;
let selection = null;
let evidenceRequest = null;
let view = "exams";

function node(tag, text) {
  const result = document.createElement(tag);
  if (text !== undefined) result.textContent = text;
  return result;
}

function notice(text = "", failed = false) {
  const target = element("notice");
  target.textContent = text;
  target.hidden = !text;
  target.classList.toggle("error", failed);
}

function report(error) {
  if (error.name !== "AbortError") notice(error.message, true);
}

function resetSession() {
  session?.controller.abort();
  session = null;
  selection = null;
  evidenceRequest = null;
  element("login-form").reset();
  element("create-form").reset();
  element("import-form").reset();
  element("exam-select").replaceChildren();
  element("exam-summary").textContent = "请选择一场考试";
  element("refresh-dashboard").disabled = false;
  element("refresh-dashboard").textContent = "刷新";
  renderRoster([], null);
  clearEvidence("请选择考试");
  element("dashboard").hidden = true;
  element("logout").hidden = true;
  element("login").hidden = false;
  element("token").focus();
  notice();
}

async function request(path, { method = "GET", data, allowMissing = false } = {}) {
  const active = session;
  if (!active) throw new DOMException("Session ended", "AbortError");
  const headers = { Authorization: `Bearer ${active.token}` };
  if (data !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(`/api/admin${path}`, {
    method, headers, body: data === undefined ? undefined : JSON.stringify(data),
    cache: "no-store", credentials: "omit", redirect: "error", signal: active.controller.signal,
  });
  active.controller.signal.throwIfAborted();
  if (response.status === 401) {
    resetSession();
    notice(errorMessages.ADMIN_AUTH_REQUIRED, true);
    throw new Error(errorMessages.ADMIN_AUTH_REQUIRED);
  }
  if (response.status === 404 && allowMissing) return null;
  if (response.status === 204) return null;
  const result = await response.json().catch(() => null);
  active.controller.signal.throwIfAborted();
  if (!response.ok) {
    const error = new Error(errorMessages[result?.code] ?? `请求失败（HTTP ${response.status}）。`);
    error.code = result?.code;
    throw error;
  }
  if (result === null) throw new Error("服务端返回了无效的响应。");
  return result;
}

function option(value, label) {
  const item = node("option", label);
  item.value = value;
  return item;
}

function showExams(exams, preferred) {
  const picker = element("exam-select");
  const retained = preferred ?? picker.value;
  picker.replaceChildren(...exams.map((exam) => option(exam.id, `${exam.title} · ${phases[exam.phase] ?? exam.phase}`)));
  if (exams.some((exam) => exam.id === retained)) picker.value = retained;
  if (!exams.length) picker.append(option("", "暂无考试"));
  if (picker.value !== selection?.id) chooseExam();
}

function chooseExam() {
  const id = element("exam-select").value;
  selection = id ? { id, pending: false, rendered: "" } : null;
  element("exam-summary").textContent = id ? "正在加载考试…" : "暂无考试，可在下方创建";
  element("poll-status").textContent = "每 5 秒刷新";
  element("student-select").value = "";
  renderRoster([], null);
  clearEvidence(id ? "正在加载…" : "请选择考试");
  if (id) {
    refreshRoster().catch(report);
    if (view === "evidence") loadEvidence();
  }
}

function selectView(name) {
  view = name;
  for (const button of document.querySelectorAll("[data-view]")) {
    const active = button.dataset.view === name;
    button.setAttribute("aria-pressed", active);
    element(button.dataset.view).hidden = !active;
  }
}

async function refreshDashboard() {
  const active = session;
  const button = element("refresh-dashboard");
  if (!active || button.disabled) return;
  button.disabled = true;
  button.textContent = "刷新中…";
  try {
    const exams = await request("/exams");
    if (active !== session) return;
    const current = selection;
    showExams(exams);
    if (current && selection === current) {
      await Promise.all([
        refreshRoster(true),
        view === "evidence" ? loadEvidence() : null,
      ]);
    }
  } catch (error) {
    if (active === session) report(error);
  } finally {
    if (active === session) {
      button.disabled = false;
      button.textContent = "刷新";
    }
  }
}

function renderRoster(rows, exam, studentEnrollments = []) {
  const enrollments = [];
  const tableRows = [];
  const picker = element("student-select");
  const previousStudent = picker.value;
  const options = [option("", "全部考生（仅异常）")];
  const numbersByUser = new Map();
  const pending = [];
  for (const entry of studentEnrollments) {
    if (entry.oj_user_id !== null) {
      numbersByUser.set(entry.oj_user_id, entry.student_number);
    } else if (entry.enrolled) {
      pending.push({ ...entry, runtime: null });
    }
  }
  for (const row of [...pending, ...rows]) {
    const id = row.oj_user_id;
    const number = row.student_number ?? numbersByUser.get(id);
    const label = number ?? id;
    if (row.enrolled) {
      const item = node("li", id === null ? label + "（待绑定）" : label);
      const remove = node("button", "撤销报名");
      remove.disabled = exam?.phase !== "scheduled";
      remove.addEventListener("click", async () => {
        if (!window.confirm(`撤销 ${label} 的报名？`)) return;
        const current = selection;
        remove.disabled = true;
        try {
          const entry = id === null ? `student-enrollments/${encodeURIComponent(number)}` : `participants/${id}`;
          await request(`/exams/${current.id}/${entry}`, { method: "DELETE" });
          if (current === selection) notice(`已撤销 ${label} 的报名。`);
          await refreshRoster(true);
        } catch (error) {
          if (id === null && error.code === "CONFLICT" && current === selection) {
            notice("报名状态已变化，请在刷新后的名单中再次撤销。", true);
            await refreshRoster(true).catch(report);
          } else {
            report(error);
          }
        } finally {
          remove.disabled = exam?.phase !== "scheduled";
        }
      });
      item.append(remove);
      enrollments.push(item);
    }
    const tableRow = node("tr");
    const runtime = row.runtime;
    const connection = runtime ? (runtime.connected ? "在线" : "离线") : "未连接过";
    const enrollment = row.enrolled ? (id === null ? "待绑定" : "已报名") : "已撤销";
    for (const value of [label, enrollment, connection,
      runtime ? (stages[runtime.execution] ?? runtime.execution) : "—", formatTime(runtime?.last_seen_at)]) {
      tableRow.append(node("td", value));
    }
    const detail = node("td");
    const open = node("button", "查看记录");
    open.disabled = !runtime;
    if (runtime) options.push(option(id, label));
    open.addEventListener("click", () => {
      selectView("evidence");
      picker.value = id;
      loadEvidence();
    });
    detail.append(open);
    tableRow.append(detail);
    tableRows.push(tableRow);
  }
  if (!tableRows.length) {
    const empty = node("tr");
    const cell = node("td", exam ? "暂无考生记录" : "请先选择考试");
    cell.colSpan = 6;
    empty.append(cell);
    tableRows.push(empty);
  }
  element("enrollments").replaceChildren(...(enrollments.length ? enrollments : [node("li", "暂无报名名单")]));
  element("student-rows").replaceChildren(...tableRows);
  element("import-form").querySelector("button").disabled = !exam;
  picker.replaceChildren(...options);
  if (options.some((item) => item.value === previousStudent)) picker.value = previousStudent;
}

async function refreshRoster(force = false) {
  const current = selection;
  if (!current) return;
  if (current.pending) {
    if (force) current.repeat = true;
    return;
  }
  current.pending = true;
  try {
    const [detail, rows, studentEnrollments] = await Promise.all([
      request(`/exams/${current.id}`), request(`/exams/${current.id}/roster`),
      request(`/exams/${current.id}/student-enrollments`),
    ]);
    if (current !== selection) return;
    const exam = detail.summary;
    element("exam-summary").textContent = `${exam.title} · ${phases[exam.phase] ?? exam.phase} · ${formatTime(exam.start_at)} 至 ${formatTime(exam.end_at)} · ${detail.policy.fps} FPS / ${detail.policy.segment_duration_secs} 秒分片`;
    element("poll-status").textContent = `更新于 ${formatTime(Date.now())} · 每 5 秒刷新`;
    const key = JSON.stringify([exam.phase, rows, studentEnrollments]);
    if (key !== current.rendered) {
      renderRoster(rows, exam, studentEnrollments);
      current.rendered = key;
    }
  } catch (error) {
    if (current !== selection || error.name === "AbortError") return;
    element("poll-status").textContent = "刷新失败，显示上次成功读取的状态";
    throw error;
  } finally {
    current.pending = false;
    if (current === selection && current.repeat) {
      current.repeat = false;
      refreshRoster().catch(report);
    }
  }
}

function clearEvidence(message) {
  element("incidents").replaceChildren(node("li", message));
  element("events").replaceChildren(node("li", message));
  element("recording").textContent = message;
}

function evidenceIdentity(id, identities) {
  const identity = identities.get(id);
  const number = identity?.student_number ?? `账号 ${id}（学号未登记）`;
  return `${number} · ${identity?.nickname ?? "昵称待同步"}`;
}

async function loadEvidence() {
  const current = selection;
  const marker = {};
  evidenceRequest = marker;
  if (!current) return clearEvidence("请先选择考试");
  const id = element("student-select").value;
  const query = id ? `?oj_user_id=${id}` : "";
  const studentPath = `/exams/${current.id}/students/${id}`;
  clearEvidence("正在加载…");
  try {
    const [incidents, events, recording, students] = await Promise.all([
      request(`/exams/${current.id}/incidents${query}`),
      id ? request(`${studentPath}/events`) : [],
      id ? request(`${studentPath}/recording`, { allowMissing: true }) : null,
      request(`/exams/${current.id}/student-identities`),
    ]);
    if (current !== selection || marker !== evidenceRequest) return;
    const identities = new Map(students.map((student) => [student.oj_user_id, student]));
    element("incidents").replaceChildren(...(incidents.length ? incidents.map((item) => node("li",
      `${formatTime(item.occurred_at)} · ${evidenceIdentity(item.oj_user_id, identities)} · ${item.source === "server" ? "服务端" : "客户端"} · ${item.kind}${item.detail ? ` · ${item.detail}` : ""}`
    )) : [node("li", "暂无异常记录")]));
    element("events").replaceChildren(...(events.length ? events.map((item) => {
      const entry = node("li", `#${item.seq} · ${formatTime(item.received_at)} · ${evidenceIdentity(id, identities)}`);
      const details = node("details");
      details.append(node("summary", item.event.kind), node("pre", JSON.stringify(item.event, null, 2)));
      entry.append(details);
      return entry;
    }) : [node("li", id ? "暂无上报事件" : "选择一位考生以查看事件")]));
    element("recording").textContent = recording ? `${recording.segment_count} 个分片 · SHA-256：${recording.merged_sha256}` : id ? "暂无录像摘要" : "选择一位考生以查看录像摘要";
  } catch (error) {
    if (current !== selection || marker !== evidenceRequest) return;
    clearEvidence("读取失败，可点击刷新重试");
    report(error);
  }
}

function handleForm(id, action) {
  element(id).addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const button = form.querySelector("button");
    if (button.disabled) return;
    button.disabled = true;
    try {
      notice();
      await action(new FormData(form), form);
    } catch (error) {
      report(error);
    } finally {
      button.disabled = id === "import-form" && !selection;
    }
  });
}

handleForm("login-form", async () => {
  session = { token: element("token").value.trim(), controller: new AbortController() };
  element("token").value = "";
  const active = session;
  try {
    const exams = await request("/exams");
    element("login").hidden = true;
    element("dashboard").hidden = false;
    element("logout").hidden = false;
    selectView("exams");
    showExams(exams);
    element("exam-select").focus();
  } catch (error) {
    if (active === session) resetSession();
    throw error;
  }
});

handleForm("create-form", async (fields, form) => {
  const start = new Date(fields.get("start"));
  const end = new Date(fields.get("end"));
  if (!(start < end)) throw new Error("结束时间必须晚于开考时间。");
  const result = await request("/exams", { method: "POST", data: {
    title: fields.get("title"), start_at: start.toISOString(), end_at: end.toISOString(),
    policy: { segment_duration_secs: Number(fields.get("duration")), fps: Number(fields.get("fps")) },
  } });
  form.reset();
  notice("考试已创建。");
  try {
    showExams(await request("/exams"), result.exam_id);
  } catch (error) {
    if (session && error.name !== "AbortError") {
      notice("考试已创建。列表刷新失败，请点击页面上的「刷新」重试。", true);
    }
  }
});

handleForm("import-form", async (fields, form) => {
  const current = selection;
  if (!current) return;
  const studentNumbers = fields.get("student_numbers").trim().split(/[\s,，]+/);
  if (studentNumbers.length > 1000 || studentNumbers.some((number) => !/^[0-9]{1,20}$/.test(number))) {
    throw new Error("请输入 1–1,000 个学号，每个学号为 1–20 位数字。");
  }
  const result = await request(`/exams/${current.id}/student-enrollments`, {
    method: "POST", data: { student_numbers: studentNumbers },
  });
  if (current !== selection) return;
  form.reset();
  notice(`已添加 ${result.count} 人，重复报名已跳过。`);
  await refreshRoster(true);
});

element("logout").addEventListener("click", resetSession);
element("refresh-dashboard").addEventListener("click", refreshDashboard);
element("exam-select").addEventListener("change", chooseExam);
element("student-select").addEventListener("change", loadEvidence);
element("refresh-evidence").addEventListener("click", loadEvidence);
for (const button of document.querySelectorAll("[data-view]")) {
  button.addEventListener("click", () => {
    selectView(button.dataset.view);
    if (view === "evidence") loadEvidence();
  });
}
setInterval(() => {
  if (!document.hidden && selection) refreshRoster().catch(report);
}, 5000);
