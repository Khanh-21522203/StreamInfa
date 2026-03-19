import http from "k6/http";
import { check, sleep } from "k6";
import { Rate, Trend } from "k6/metrics";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const streamId = __ENV.DELIVERY_STREAM_ID || "";
const rendition = __ENV.DELIVERY_RENDITION || "high";
const segment = __ENV.DELIVERY_SEGMENT || "000000.m4s";
const useRange = __ENV.DELIVERY_USE_RANGE !== "0";
const duration = __ENV.DURATION || "90s";
const vus = Number(__ENV.VUS || 80);

if (!streamId) {
  throw new Error("DELIVERY_STREAM_ID is required for fault benchmark");
}

export const faultSuccessRate = new Rate("fault_success_rate");
export const faultLoopMs = new Trend("fault_loop_ms");

export const options = {
  scenarios: {
    fault_delivery_path: {
      executor: "constant-vus",
      exec: "faultPath",
      vus,
      duration,
      gracefulStop: "5s",
    },
  },
  thresholds: {
    http_req_failed: ["rate<0.25"],
    fault_success_rate: ["rate>0.7"],
  },
};

export function faultPath() {
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
    "fault: master playlist status is 200": (r) => r.status === 200,
  }) &&
    check(mediaRes, {
      "fault: media playlist status is 200": (r) => r.status === 200,
    }) &&
    check(initRes, {
      "fault: init segment status is 200": (r) => r.status === 200,
    }) &&
    check(segmentRes, {
      "fault: segment status is 200 or 206": (r) => r.status === 200 || r.status === 206,
    });

  faultSuccessRate.add(ok);
  faultLoopMs.add(Date.now() - startedAt);
  sleep(0.05);
}
