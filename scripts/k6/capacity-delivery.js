import http from "k6/http";
import { check, sleep } from "k6";

const baseUrl = __ENV.BASE_URL || "http://localhost:8080";
const streamId = __ENV.DELIVERY_STREAM_ID || "";
const rendition = __ENV.DELIVERY_RENDITION || "high";
const segment = __ENV.DELIVERY_SEGMENT || "000000.m4s";
const useRange = __ENV.DELIVERY_USE_RANGE !== "0";
const duration = __ENV.CAPACITY_DURATION || __ENV.DURATION || "20s";
const vus = Number(__ENV.VUS || __ENV.CAPACITY_VUS || 10);
const p95LimitMs = Number(__ENV.CAPACITY_P95_MS || 400);
const maxErrorRate = Number(__ENV.CAPACITY_ERROR_RATE_MAX || 0.01);

if (!streamId) {
  throw new Error("DELIVERY_STREAM_ID is required for capacity benchmark");
}

export const options = {
  scenarios: {
    capacity_delivery_read_path: {
      executor: "constant-vus",
      exec: "capacityReadPath",
      vus,
      duration,
      gracefulStop: "5s",
    },
  },
  thresholds: {
    http_req_failed: [`rate<${maxErrorRate}`],
    http_req_duration: [`p(95)<${p95LimitMs}`],
  },
};

export function capacityReadPath() {
  const masterRes = http.get(`${baseUrl}/streams/${streamId}/master.m3u8`);
  check(masterRes, {
    "capacity: master playlist status is 200": (r) => r.status === 200,
  });

  const mediaRes = http.get(
    `${baseUrl}/streams/${streamId}/${rendition}/media.m3u8`,
  );
  check(mediaRes, {
    "capacity: media playlist status is 200": (r) => r.status === 200,
  });

  const initRes = http.get(
    `${baseUrl}/streams/${streamId}/${rendition}/init.mp4`,
  );
  check(initRes, {
    "capacity: init segment status is 200": (r) => r.status === 200,
  });

  const segmentUrl = `${baseUrl}/streams/${streamId}/${rendition}/${segment}`;
  const segmentRes = useRange
    ? http.get(segmentUrl, { headers: { Range: "bytes=0-65535" } })
    : http.get(segmentUrl);

  check(segmentRes, {
    "capacity: segment status is 200 or 206": (r) => r.status === 200 || r.status === 206,
  });
  sleep(0.05);
}
