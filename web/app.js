const state = {
  busy: false,
};

const $ = (id) => document.getElementById(id);

function setStatus(text, kind = "") {
  const pill = $("statusPill");
  pill.textContent = text;
  pill.className = `status-pill ${kind}`.trim();
}

function setBusy(value, label = "Working") {
  state.busy = value;
  $("operationState").textContent = value ? label : "Idle";
  for (const id of ["buildBtn", "copyBtn", "breakBtn", "runBtn", "refreshBtn"]) {
    $(id).disabled = value;
  }
}

function appendOutput(text) {
  const output = $("operationOutput");
  output.textContent = `${output.textContent}${text}\n`;
  output.scrollTop = output.scrollHeight;
}

function setOutput(text) {
  const output = $("operationOutput");
  output.textContent = text || "";
  output.scrollTop = output.scrollHeight;
}

async function requestJson(path, options = {}) {
  const response = await fetch(path, {
    headers: { "Content-Type": "application/json" },
    ...options,
  });
  const payload = await response.json();
  if (!payload.ok) {
    throw new Error(payload.error || `Request failed: ${response.status}`);
  }
  return payload.data;
}

function formatBytes(value) {
  if (value == null) return "-";
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}

function chips(container, values) {
  container.innerHTML = "";
  if (!values || values.length === 0) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = "none";
    container.appendChild(chip);
    return;
  }
  for (const value of values) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = value;
    container.appendChild(chip);
  }
}

async function refreshStatus() {
  try {
    const data = await requestJson("/api/status");
    const board = data.board;
    $("subtitle").textContent = data.board_path;
    $("boardId").textContent = board.id;
    $("boardName").textContent = board.name;
    $("serialPort").textContent = board.port;
    $("baud").textContent = board.baud;
    $("loadAddr").textContent = board.load_addr;
    $("mmc").textContent = `mmc ${board.mmc_dev}:${board.mmc_part}`;
    $("imagePath").textContent = data.image_path;
    $("imageState").textContent = data.image.exists ? "ready" : "missing";
    $("imageSize").textContent = formatBytes(data.image.size);
    $("imageHash").textContent = data.image.sha256 || "-";
    chips($("detectedPorts"), data.serial.detected_ports || []);
    chips($("volumes"), (data.volumes || []).map((volume) => volume.path));
    if (data.latest_log) {
      $("logPath").textContent = data.latest_log.path;
      $("latestLog").textContent = data.latest_log.text;
      $("latestLog").scrollTop = $("latestLog").scrollHeight;
    } else {
      $("logPath").textContent = "-";
      $("latestLog").textContent = "";
    }
    setStatus("Connected", "ok");
  } catch (error) {
    setStatus("API error", "err");
    appendOutput(error.message);
  }
}

async function buildOs() {
  setBusy(true, "Building OS");
  setOutput("");
  try {
    const result = await requestJson("/api/build-os", { method: "POST", body: "{}" });
    appendOutput(result.stdout || "");
    appendOutput(result.stderr || "");
    appendOutput(result.ok ? "Build completed." : `Build failed with code ${result.code}.`);
    await refreshStatus();
  } catch (error) {
    appendOutput(error.message);
  } finally {
    setBusy(false);
  }
}

async function copyToSd() {
  setBusy(true, "Copying image");
  try {
    const volume = $("volumeInput").value.trim() || "/Volumes/R5COS";
    const result = await requestJson("/api/sd-copy", {
      method: "POST",
      body: JSON.stringify({ volume }),
    });
    appendOutput(`Copied image to ${volume}`);
    appendOutput(`Size: ${formatBytes(result.size)}`);
    appendOutput(`SHA256: ${result.sha256 || "-"}`);
    await refreshStatus();
  } catch (error) {
    appendOutput(error.message);
  } finally {
    setBusy(false);
  }
}

async function breakUboot() {
  setBusy(true, "Breaking U-Boot");
  try {
    appendOutput("Reset or power-cycle the board now.");
    const result = await requestJson("/api/break", {
      method: "POST",
      body: JSON.stringify({}),
    });
    appendOutput(`U-Boot prompt detected. Log: ${result.log_path}`);
    await refreshLatestLog(result);
  } catch (error) {
    appendOutput(error.message);
  } finally {
    setBusy(false);
  }
}

async function runSd() {
  setBusy(true, "Running board");
  try {
    if (!$("noBreakInput").checked) {
      appendOutput("Reset or power-cycle the board now.");
    }
    const result = await requestJson("/api/run-sd", {
      method: "POST",
      body: JSON.stringify({
        no_break: $("noBreakInput").checked,
        run_seconds: Number($("runSecondsInput").value || 8),
      }),
    });
    appendOutput(`Verified Xiaojin OS. Log: ${result.log_path}`);
    await refreshLatestLog(result);
  } catch (error) {
    appendOutput(error.message);
  } finally {
    setBusy(false);
  }
}

async function refreshLatestLog(preloaded) {
  if (preloaded && preloaded.log_tail) {
    $("logPath").textContent = preloaded.log_path;
    $("latestLog").textContent = preloaded.log_tail;
    $("latestLog").scrollTop = $("latestLog").scrollHeight;
    return;
  }
  const data = await requestJson("/api/logs/latest");
  if (data) {
    $("logPath").textContent = data.path;
    $("latestLog").textContent = data.text;
    $("latestLog").scrollTop = $("latestLog").scrollHeight;
  }
}

$("refreshBtn").addEventListener("click", refreshStatus);
$("buildBtn").addEventListener("click", buildOs);
$("copyBtn").addEventListener("click", copyToSd);
$("breakBtn").addEventListener("click", breakUboot);
$("runBtn").addEventListener("click", runSd);

refreshStatus();
