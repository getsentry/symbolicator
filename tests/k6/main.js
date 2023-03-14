import http from "k6/http";
import { sleep, check } from "k6";
import { Counter } from "k6/metrics";
import { FormData } from "https://jslib.k6.io/formdata/0.0.2/index.js";

// A simple counter for http requests
export const requests = new Counter("http_reqs");

const boundary = "----MinidumpUploadBoundary";

// TODO: consider using SharedArray
const windowsDump = createFormData("../fixtures/windows.dmp");
const linuxDump = createFormData("../fixtures/linux.dmp");
const macosDump = createFormData("../fixtures/macos.dmp");

// Basic scenario configuration
export const options = {
  scenarios: {
    windows: {
      executor: "shared-iterations",
      exec: "postMinidumpWindows",
    },
    linux: {
      executor: "shared-iterations",
      exec: "postMinidumpLinux",
    },
    macos: {
      executor: "shared-iterations",
      exec: "postMinidumpMacos",
    },
  },
};

function createFormData(dump) {
  const f = open(dump, "b");
  let fd = new FormData();
  fd.boundary = boundary;
  fd.append("upload_file_minidump", {
    data: new Uint8Array(f).buffer,
  });
  return fd;
}

function postMinidump(fd) {
  const headers = {
    "Content-Type": `multipart/form-data; boundary=${fd.boundary}`,
  };

  const res = http.post(`${__ENV.HOST}/minidump`, fd.body(), {
    headers: headers,
  });
  sleep(1);

  const checkRes = check(res, {
    "response code is 200": (r) => r.status === 200,
    "status is completed": (r) => r.json("status") === "completed",
  });
}

export function postMinidumpLinux() {
  postMinidump(linuxDump);
}

export function postMinidumpWindows() {
  postMinidump(windowsDump);
}

export function postMinidumpMacos() {
  postMinidump(macosDump);
}
