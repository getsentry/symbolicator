This folder contains a few `wrk` scripts that load-test symbolicator.

So far there are two files for testing both `symbolicate` json payloads, and
`minidump` payloads.

Before starting to load-test, it is good to prime the symbolicator caches by sending
the payload at least once prior to benchmarking:

> cargo run -p process-event -- path/to/mini.dmp OR path/to/event.json

Then run wrk like this:

> WRK_MINIDUMP="path/to/mini.dmp" wrk --threads 10 --connections 50 --duration 30s --script tests/wrk/minidump.lua http://127.0.0.1:3021/minidump

OR

> WRK_EVENT="path/to/event.json" wrk --threads 10 --connections 50 --duration 30s --script tests/wrk/event.lua http://127.0.0.1:3021/symbolicate
