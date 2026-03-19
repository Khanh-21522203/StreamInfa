import http from "k6/http";
import { check, sleep } from "k6";
import { Rate, Trend } from "k6/metrics";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const adminToken = __ENV.ADMIN_TOKEN || "";
const duration = __ENV.DURATION || "30s";
const uploadVus = Number(__ENV.INGEST_UPLOAD_VUS || 2);
const readyTimeoutSecs = Number(__ENV.INGEST_READY_TIMEOUT_SECS || 120);
const pollIntervalMs = Number(__ENV.INGEST_POLL_INTERVAL_MS || 500);
const uploadFilePath = __ENV.UPLOAD_FILE_PATH || "./fixtures/upload-sample.mp4";
const deleteAfterBenchmark = __ENV.INGEST_DELETE_STREAM !== "0";

if (!adminToken) {
  throw new Error("ADMIN_TOKEN is required for ingest benchmark");
}

if (readyTimeoutSecs <= 0) {
  throw new Error("INGEST_READY_TIMEOUT_SECS must be > 0");
}

if (pollIntervalMs <= 0) {
  throw new Error("INGEST_POLL_INTERVAL_MS must be > 0");
}

const uploadBody = open(uploadFilePath, "b");

const authHeaders = {
  headers: {
    Authorization: `Bearer ${adminToken}`,
  },
};

export const vodUploadRequestDurationMs = new Trend(
  "vod_upload_request_duration_ms",
);
export const vodUploadToReadyDurationMs = new Trend(
  "vod_upload_to_ready_duration_ms",
);
export const vodReadyPollAttempts = new Trend("vod_ready_poll_attempts");
export const vodReadySuccess = new Rate("vod_ready_success");

export const options = {
  scenarios: {
    upload_vod: {
      executor: "constant-vus",
      exec: "uploadAndWaitReady",
      vus: uploadVus,
      duration,
      gracefulStop: "5s",
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.02"],
    "http_req_duration{scenario:upload_vod}": ["p(95)<3000", "p(99)<5000"],
    vod_upload_request_duration_ms: ["p(95)<3000"],
    vod_upload_to_ready_duration_ms: ["p(95)<45000"],
    vod_ready_success: ["rate>0.95"],
  },
};

function parseJsonSafely(res) {
  try {
    return res.json();
  } catch (_err) {
    return null;
  }
}

export function uploadAndWaitReady() {
  const startedAt = Date.now();

  const payload = {
    file: http.file(uploadBody, "upload-sample.mp4", "video/mp4"),
  };

  const uploadRes = http.post(`${baseUrl}/api/v1/streams/upload`, payload, authHeaders);
  vodUploadRequestDurationMs.add(uploadRes.timings.duration);

  const uploadOk = check(uploadRes, {
    "upload returns 201": (r) => r.status === 201,
  });

  if (!uploadOk) {
    vodReadySuccess.add(false);
    sleep(0.1);
    return;
  }

  const uploadBodyJson = parseJsonSafely(uploadRes);
  const streamId = uploadBodyJson && uploadBodyJson.stream_id;

  const streamIdOk = check({ streamId }, {
    "upload returns stream_id":
      (v) => typeof v.streamId === "string" && v.streamId.length > 0,
  });

  if (!streamIdOk || !streamId) {
    vodReadySuccess.add(false);
    sleep(0.1);
    return;
  }

  const deadline = Date.now() + readyTimeoutSecs * 1000;
  let pollCount = 0;
  let isReady = false;
  let lastStatus = "unknown";

  while (Date.now() < deadline) {
    const statusRes = http.get(`${baseUrl}/api/v1/streams/${streamId}`, authHeaders);
    pollCount += 1;

    if (statusRes.status === 200) {
      const detail = parseJsonSafely(statusRes);
      if (detail && typeof detail.status === "string") {
        lastStatus = detail.status;
      }

      if (lastStatus === "ready") {
        isReady = true;
        break;
      }

      if (lastStatus === "error" || lastStatus === "deleted") {
        break;
      }
    }

    sleep(pollIntervalMs / 1000);
  }

  vodReadyPollAttempts.add(pollCount);
  vodReadySuccess.add(isReady);

  if (isReady) {
    vodUploadToReadyDurationMs.add(Date.now() - startedAt);
  }

  check({ isReady, lastStatus }, {
    "stream reaches ready": (v) => v.isReady,
  });

  if (deleteAfterBenchmark) {
    const deleteRes = http.del(`${baseUrl}/api/v1/streams/${streamId}`, null, authHeaders);
    check(deleteRes, {
      "delete after ingest benchmark returns 200": (r) => r.status === 200,
    });
  }

  sleep(0.1);
}
