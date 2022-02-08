-- This is a `wrk` script that posts a multipart minidump request to symbolicator.
-- It is best to prime the symbolicator caches by posting the minidump at least once prior to benchmarking:
-- > cargo run -p process-event -- path/to/mini.dmp
-- And then run this script, giving it the minidump file via env:
-- > WRK_MINIDUMP="path/to/mini.dmp" wrk --threads 20 --connections 50 --duration 30s --script wrk_minidump.lua http://127.0.0.1:3021/minidump

function read_file(path)
  local file, errorMessage = io.open(path, "rb")
  if not file then
      error("Could not read the file:" .. errorMessage .. "\n")
  end

  local content = file:read "*all"
  file:close()
  return content
end

local Boundary = "----MinidumpUploadBoundary"
local BodyBoundary = "--" .. Boundary
local LastBoundary = "--" .. Boundary .. "--"
local CRLF = "\r\n"
local MinidumpFile = os.getenv("WRK_MINIDUMP")
local FileBody = read_file(MinidumpFile)
local ContentDisposition = 'Content-Disposition: form-data; name="upload_file_minidump"'

wrk.method = "POST"
wrk.path = "/minidump"
wrk.headers["Content-Type"] = "multipart/form-data; boundary=" .. Boundary
wrk.body = BodyBoundary .. CRLF .. ContentDisposition .. CRLF .. CRLF .. FileBody .. CRLF .. LastBoundary
