import http from "k6/http";
import { check } from "k6";
import { Rate, Trend } from "k6/metrics";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const adminToken = __ENV.ADMIN_TOKEN || "";
const streamId = __ENV.DELIVERY_STREAM_ID || "";
const rendition = __ENV.DELIVERY_RENDITION || "high";
const segment = __ENV.DELIVERY_SEGMENT || "000000.m4s";
const useRange = __ENV.DELIVERY_USE_RANGE !== "0";

const duration = __ENV.OVERLOAD_DURATION || __ENV.DURATION || "120s";
const deliveryVus = Number(__ENV.OVERLOAD_VUS || __ENV.VUS || 80);
const controlCreateVus = Number(
  __ENV.OVERLOAD_CONTROL_CREATE_VUS || __ENV.CONTROL_CREATE_VUS || 8,
);

if (!streamId) {
  throw new Error("DELIVERY_STREAM_ID is required for overload benchmark");
}

const authHeaders = {
  headers: {
    Authorization: `Bearer ${adminToken}`,
    "Content-Type": "application/json",
  },
};

export const overloadLoopMs = new Trend("overload_loop_ms");
export const overloadSuccessRate = new Rate("overload_success_rate");
export const overloadCreateDeleteMs = new Trend("overload_create_delete_ms");

const scenarios = {
  delivery_overload: {
    executor: "constant-vus",
    exec: "deliveryOverload",
    vus: deliveryVus,
    duration,
    gracefulStop: "5s",
  },
};

if (adminToken) {
  scenarios.control_churn = {
    executor: "constant-vus",
    exec: "controlChurn",
    vus: controlCreateVus,
    duration,
    gracefulStop: "5s",
  };
}

export const options = {
  scenarios,
  thresholds: {
    http_req_failed: ["rate<0.15"],
    "http_req_duration{scenario:delivery_overload}": ["p(95)<1500", "p(99)<3000"],
    overload_success_rate: ["rate>0.85"],
  },
};

export function deliveryOverload() {
  const startedAt = Date.now();

  const masterRes = http.get(`${baseUrl}/streams/${streamId}/master.m3u8`);
  const mediaRes = http.get(
    `${baseUrl}/streams/${streamId}/${rendition}/media.m3u8`,
  );
  const initRes = http.get(
    `${baseUrl}/streams/${streamId}/${rendition}/init.mp4`,
  );

  const segmentUrl = `${baseUrl}/streams/${streamId}/${rendition}/${segment}`;
  const segmentRes = useRange
    ? http.get(segmentUrl, { headers: { Range: "bytes=0-65535" } })
    : http.get(segmentUrl);

  const ok = check(masterRes, {
    "overload: master playlist status is 200": (r) => r.status === 200,
  }) &&
    check(mediaRes, {
      "overload: media playlist status is 200": (r) => r.status === 200,
    }) &&
    check(initRes, {
      "overload: init segment status is 200": (r) => r.status === 200,
    }) &&
    check(segmentRes, {
      "overload: segment status is 200 or 206":
        (r) => r.status === 200 || r.status === 206,
    });

  overloadSuccessRate.add(ok);
  overloadLoopMs.add(Date.now() - startedAt);
}

export function controlChurn() {
  if (!adminToken) {
    return;
  }

  const startedAt = Date.now();
  const payload = JSON.stringify({
    ingest_type: "rtmp",
    metadata: { benchmark: "k6-overload" },
  });
  const createRes = http.post(`${baseUrl}/api/v1/streams`, payload, authHeaders);

  const created = check(createRes, {
    "overload: create stream status is 200 or 201":
      (r) => r.status === 200 || r.status === 201,
  });

  if (!created) {
    overloadSuccessRate.add(false);
    return;
  }

  const body = createRes.json();
  const streamIdCreated = body && body.stream_id;
  if (!streamIdCreated) {
    overloadSuccessRate.add(false);
    return;
  }

  const deleteRes = http.del(`${baseUrl}/api/v1/streams/${streamIdCreated}`, null, {
    headers: { Authorization: `Bearer ${adminToken}` },
  });
  const deleted = check(deleteRes, {
    "overload: delete stream status is 200": (r) => r.status === 200,
  });
  overloadSuccessRate.add(deleted);
  overloadCreateDeleteMs.add(Date.now() - startedAt);
}
