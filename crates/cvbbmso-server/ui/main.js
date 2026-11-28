"use strict";

const element = (id) => document.getElementById(id);
const formatTime = (value) => value ? new Date(value).toLocaleString("zh-CN") : "—";
const phases = { scheduled: "未开始", running: "进行中", ended: "已结束" };
const stages = { ready: "就绪", recording: "录像中", finalizing: "收尾中", finished: "已结束" };
const errorMessages = {
  ADMIN_AUTH_REQUIRED: "管理员令牌无效或已轮换，请重新登录。",
  INVALID_INPUT: "输入无效，请检查考试时间、录像参数或用户 ID。",
  NOT_FOUND: "未找到对应记录。",
  CONFLICT: "考试状态已变化，当前操作无法执行。",
  STORAGE_BUSY: "服务端正在处理其他写入，请稍后重试。",
  BODY_TOO_LARGE: "导入内容过大，请分批操作。",
};
let session = null;
let selection = null;
let evidenceRequest = null;
let eventState = null;
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
  if (!response.ok) throw new Error(errorMessages[result?.code] ?? `请求失败（HTTP ${response.status}）。`);
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
  chooseExam();
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

function renderRoster(rows, exam) {
  const enrollments = [];
  const tableRows = [];
  const picker = element("student-select");
  const previousStudent = picker.value;
  const options = [option("", "全部考生（仅异常）")];
  for (const row of rows) {
    const id = row.oj_user_id;
    if (row.enrolled) {
      const item = node("li", id);
      const remove = node("button", "撤销报名");
      remove.disabled = exam?.phase !== "scheduled";
      remove.addEventListener("click", async () => {
        if (!window.confirm(`撤销用户 ${id} 的报名？`)) return;
        const current = selection;
        remove.disabled = true;
        try {
          await request(`/exams/${current.id}/participants/${id}`, { method: "DELETE" });
          if (current === selection) notice(`已撤销用户 ${id} 的报名。`);
          await refreshRoster(true);
        } catch (error) {
          report(error);
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
    for (const value of [id, row.enrolled ? "已报名" : "已撤销", connection,
      runtime ? (stages[runtime.execution] ?? runtime.execution) : "—", formatTime(runtime?.last_seen_at)]) {
      tableRow.append(node("td", value));
    }
    const detail = node("td");
    const open = node("button", "查看记录");
    open.disabled = !runtime;
    if (runtime) options.push(option(id, id));
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
    const [detail, rows] = await Promise.all([
      request(`/exams/${current.id}`), request(`/exams/${current.id}/roster`),
    ]);
    if (current !== selection) return;
    const exam = detail.summary;
    element("exam-summary").textContent = `${exam.title} · ${phases[exam.phase] ?? exam.phase} · ${formatTime(exam.start_at)} 至 ${formatTime(exam.end_at)} · ${detail.policy.fps} FPS / ${detail.policy.segment_duration_secs} 秒分片`;
    element("poll-status").textContent = `更新于 ${formatTime(Date.now())} · 每 5 秒刷新`;
    const key = JSON.stringify([exam.phase, rows]);
    if (key !== current.rendered) {
      renderRoster(rows, exam);
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
  eventState = null;
  element("event-pagination").hidden = true;
  element("event-refresh-status").textContent = "";
  element("incidents").replaceChildren(node("li", message));
  element("events").replaceChildren(node("li", message));
  element("recording").textContent = message;
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
  if (id) {
    eventState = { path: `${studentPath}/events`, cursors: [null], next: null };
    loadEventPage();
  } else {
    element("events").replaceChildren(node("li", "选择一位考生以查看事件"));
  }
  try {
    const [incidents, recording] = await Promise.all([
      request(`/exams/${current.id}/incidents${query}`),
      id ? request(`${studentPath}/recording`, { allowMissing: true }) : null,
    ]);
    if (current !== selection || marker !== evidenceRequest) return;
    element("incidents").replaceChildren(...(incidents.length ? incidents.map((item) => node("li",
      `${formatTime(item.occurred_at)} · ${item.oj_user_id} · ${item.source === "server" ? "服务端" : "客户端"} · ${item.kind}${item.detail ? ` · ${item.detail}` : ""}`
    )) : [node("li", "暂无异常记录")]));
    element("recording").textContent = recording ? `${recording.segment_count} 个分片 · SHA-256：${recording.merged_sha256}` : id ? "暂无录像摘要" : "选择一位考生以查看录像摘要";
  } catch (error) {
    if (current !== selection || marker !== evidenceRequest) return;
    element("incidents").replaceChildren(node("li", "读取失败，可点击刷新重试"));
    element("recording").textContent = "读取失败，可点击刷新重试";
    report(error);
  }
}

function renderEventControls(state, message) {
  const page = state.cursors.length;
  element("event-pagination").hidden = !state.loaded;
  element("event-page-number").textContent = `第 ${page} 页`;
  element("event-latest").disabled = page === 1 || state.navigating;
  element("event-newer").disabled = page === 1 || state.navigating;
  element("event-older").disabled = !state.next || state.navigating;
  element("event-refresh-status").textContent = message;
}

function renderEvents(items) {
  const list = element("events");
  const existing = new Map([...list.children].map((entry) => [entry.dataset.seq, entry]));
  const entries = items.map((item) => {
    if (existing.has(item.seq)) return existing.get(item.seq);
    const entry = node("li", `#${item.seq} · ${formatTime(item.received_at)}`);
    entry.dataset.seq = item.seq;
    const details = node("details");
    details.append(node("summary", item.event.kind), node("pre", JSON.stringify(item.event, null, 2)));
    entry.append(details);
    return entry;
  });
  if (!entries.length) entries.push(node("li", "暂无上报事件"));
  if (entries.length !== list.children.length || entries.some((entry, index) => entry !== list.children[index])) {
    list.replaceChildren(...entries);
  }
}

async function loadEventPage(cursors, automatic = false) {
  const state = eventState;
  if (!state || state.navigating || (automatic && state.pending)) return;
  const target = cursors ?? state.cursors;
  const before = target.at(-1);
  const marker = {};
  state.pending = marker;
  state.navigating = !automatic;
  const status = before ? "历史页 · 不自动刷新" : "最新事件在上 · 每 5 秒刷新";
  if (!automatic) renderEventControls(state, "正在加载事件…");
  let message = status;
  try {
    const page = await request(state.path + (before ? `?before=${before}` : ""));
    if (state !== eventState || marker !== state.pending) return;
    renderEvents(page.items);
    state.cursors = target;
    state.next = page.next_before;
    state.loaded = true;
  } catch (error) {
    if (state !== eventState || marker !== state.pending) return;
    message = state.loaded ? "刷新失败 · 保留上次结果" : "读取失败 · 可点击刷新重试";
    if (!state.loaded) element("events").replaceChildren(node("li", message));
  } finally {
    if (state === eventState && marker === state.pending) {
      state.pending = null;
      state.navigating = false;
      renderEventControls(state, message);
    }
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
      notice("考试已创建，但列表未能刷新；请重新登录后查看。", true);
    }
  }
});

handleForm("import-form", async (fields, form) => {
  const current = selection;
  if (!current) return;
  const ids = fields.get("ids").trim().split(/[\s,，]+/);
  if (ids.length > 1000 || ids.some((id) => !/^[1-9][0-9]{0,18}$/.test(id))) {
    throw new Error("请输入 1–1,000 个有效的正整数用户 ID。");
  }
  const result = await request(`/exams/${current.id}/participants`, { method: "POST", data: { oj_user_ids: ids } });
  if (current !== selection) return;
  form.reset();
  notice(`新增 ${result.count} 人，重复报名已跳过。`);
  await refreshRoster(true);
});

element("logout").addEventListener("click", resetSession);
element("exam-select").addEventListener("change", chooseExam);
element("student-select").addEventListener("change", loadEvidence);
element("refresh-evidence").addEventListener("click", loadEvidence);
element("event-latest").addEventListener("click", () => loadEventPage([null]));
element("event-newer").addEventListener("click", () => loadEventPage(eventState.cursors.slice(0, -1)));
element("event-older").addEventListener("click", () => loadEventPage([...eventState.cursors, eventState.next]));
for (const button of document.querySelectorAll("[data-view]")) {
  button.addEventListener("click", () => {
    selectView(button.dataset.view);
    if (view === "evidence") loadEvidence();
  });
}
setInterval(() => {
  if (document.hidden) return;
  if (selection) refreshRoster().catch(report);
  if (view === "evidence" && eventState?.cursors.length === 1) loadEventPage(undefined, true);
}, 5000);
