import http from "k6/http";
import { check, sleep } from "k6";
import { Rate, Trend } from "k6/metrics";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const adminToken = __ENV.ADMIN_TOKEN || "";
const streamId = __ENV.DELIVERY_STREAM_ID || "";
const rendition = __ENV.DELIVERY_RENDITION || "high";
const segment = __ENV.DELIVERY_SEGMENT || "000000.m4s";
const useRange = __ENV.DELIVERY_USE_RANGE !== "0";

const duration = __ENV.SOAK_DURATION || __ENV.DURATION || "10m";
const deliveryVus = Number(__ENV.SOAK_DELIVERY_VUS || __ENV.VUS || 20);
const controlVus = Number(__ENV.SOAK_CONTROL_VUS || 2);

if (!adminToken) {
  throw new Error("ADMIN_TOKEN is required for soak benchmark");
}
if (!streamId) {
  throw new Error("DELIVERY_STREAM_ID is required for soak benchmark");
}

const authHeaders = {
  headers: {
    Authorization: `Bearer ${adminToken}`,
    "Content-Type": "application/json",
  },
};

export const soakDeliveryLoopMs = new Trend("soak_delivery_loop_ms");
export const soakControlLoopMs = new Trend("soak_control_loop_ms");
export const soakSuccessRate = new Rate("soak_success_rate");

export const options = {
  scenarios: {
    delivery_soak: {
      executor: "constant-vus",
      exec: "deliverySoak",
      vus: deliveryVus,
      duration,
      gracefulStop: "10s",
    },
    control_probe: {
      executor: "constant-vus",
      exec: "controlProbe",
      vus: controlVus,
      duration,
      gracefulStop: "10s",
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.02"],
    "http_req_duration{scenario:delivery_soak}": ["p(95)<600", "p(99)<1500"],
    "http_req_duration{scenario:control_probe}": ["p(95)<800"],
    soak_success_rate: ["rate>0.98"],
  },
};

export function deliverySoak() {
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
    "soak: master playlist status is 200": (r) => r.status === 200,
  }) &&
    check(mediaRes, {
      "soak: media playlist status is 200": (r) => r.status === 200,
    }) &&
    check(initRes, {
      "soak: init segment status is 200": (r) => r.status === 200,
    }) &&
    check(segmentRes, {
      "soak: segment status is 200 or 206": (r) => r.status === 200 || r.status === 206,
    });

  soakSuccessRate.add(ok);
  soakDeliveryLoopMs.add(Date.now() - startedAt);
  sleep(0.1);
}

export function controlProbe() {
  const startedAt = Date.now();
  const listRes = http.get(
    `${baseUrl}/api/v1/streams?limit=20&offset=0`,
    authHeaders,
  );
  const ok = check(listRes, {
    "soak: list streams status is 200": (r) => r.status === 200,
  });
  soakSuccessRate.add(ok);
  soakControlLoopMs.add(Date.now() - startedAt);
  sleep(0.2);
}
