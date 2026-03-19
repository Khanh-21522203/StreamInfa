import http from "k6/http";
import { check, sleep } from "k6";
import { Trend } from "k6/metrics";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const adminToken = __ENV.ADMIN_TOKEN || "";
const duration = __ENV.DURATION || "30s";
const listVus = Number(__ENV.VUS || 20);
const createDeleteVus = Number(__ENV.CONTROL_CREATE_VUS || 5);

if (!adminToken) {
  throw new Error("ADMIN_TOKEN is required for control-plane benchmark");
}

const authHeaders = {
  headers: {
    Authorization: `Bearer ${adminToken}`,
    "Content-Type": "application/json",
  },
};

export const createDeleteLatency = new Trend(
  "control_create_delete_duration_ms",
);

export const options = {
  scenarios: {
    list_streams: {
      executor: "constant-vus",
      exec: "listStreams",
      vus: listVus,
      duration,
      gracefulStop: "5s",
    },
    create_delete_stream: {
      executor: "constant-vus",
      exec: "createAndDeleteStream",
      vus: createDeleteVus,
      duration,
      gracefulStop: "5s",
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.01"],
    "http_req_duration{scenario:list_streams}": ["p(95)<300", "p(99)<500"],
    "http_req_duration{scenario:create_delete_stream}": [
      "p(95)<800",
      "p(99)<1500",
    ],
  },
};

export function listStreams() {
  const res = http.get(
    `${baseUrl}/api/v1/streams?limit=20&offset=0`,
    authHeaders,
  );
  check(res, {
    "list streams status is 200": (r) => r.status === 200,
  });
  sleep(0.1);
}

export function createAndDeleteStream() {
  const startedAt = Date.now();
  const payload = JSON.stringify({
    ingest_type: "rtmp",
    metadata: { benchmark: "k6-control" },
  });

  const createRes = http.post(`${baseUrl}/api/v1/streams`, payload, authHeaders);
  const created = check(createRes, {
    "create stream status is 200 or 201":
      (r) => r.status === 200 || r.status === 201,
  });
  if (!created) {
    return;
  }

  const body = createRes.json();
  const streamId = body && body.stream_id;
  check({ streamId }, {
    "create stream returns stream_id":
      (v) => typeof v.streamId === "string" && v.streamId.length > 0,
  });
  if (!streamId) {
    return;
  }

  const deleteRes = http.del(`${baseUrl}/api/v1/streams/${streamId}`, null, {
    headers: { Authorization: `Bearer ${adminToken}` },
  });
  check(deleteRes, {
    "delete stream status is 200": (r) => r.status === 200,
  });

  createDeleteLatency.add(Date.now() - startedAt);
  sleep(0.1);
}
