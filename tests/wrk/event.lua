-- > WRK_EVENT="path/to/event.json" wrk --threads 10 --connections 50 --duration 30s --script tests/wrk/event.lua http://127.0.0.1:3021/symbolicate

function read_file(path)
  local file, errorMessage = io.open(path, "rb")
  if not file then
      error("Could not read the file:" .. errorMessage .. "\n")
  end

  local content = file:read "*all"
  file:close()
  return content
end

local JsonFile = os.getenv("WRK_EVENT")
local JsonBody = read_file(JsonFile)

wrk.method = "POST"
wrk.path = "/symbolicate"
wrk.headers["Content-Type"] = "application/json"
wrk.body = JsonBody
