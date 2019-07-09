---
title: Symbol Server Proxy
---

# Symbol Server Proxy

If the symstore proxy is enabled, Symbolicator also acts as a symbol proxy.
This means that all configured sources are probed for symbol queries below the
`/symbols` prefix. The path following this prefix needs to be a valid [SSQP
query].

Example:

```
$ curl -IL http://localhost:3021/symbols/wkernel32.pdb/ff9f9f7841db88f0cdeda9e1e9bff3b51/wkernel32.pdb
HTTP/1.1 200 OK
content-length: 846848
content-type: application/octet-stream
date: Fri, 19 Apr 2019 22:47:54 GMT
```

When fetching ELF or MachO symbols the filename can be largely omitted (non
extension can be substituted with an underscore) when a configured backend uses
the "native" directory format. In simple terms this means that
`/symbols/_/elf-buildid-180a373d6afbabf0eb1f09be1bc45bd796a71085/_` is a valid
query for an ELF executable and
`/symbols/_.debug/elf-buildid-sym-180a373d6afbabf0eb1f09be1bc45bd796a71085/_.debug`
is a valid query for an ELF debug symbol.

[ssqp query]: https://github.com/dotnet/symstore/blob/master/docs/specs/SSQP_Key_Conventions.md
