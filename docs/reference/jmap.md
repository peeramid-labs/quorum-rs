# JMAP types

`noolog-jmap` ([crate](../../crates/noolog-jmap/README.md)): JMAP wire types
written from the RFC texts. No code is taken from other JMAP implementations.

## Coverage

| RFC | Section | Types |
|---|---|---|
| 8620 | §1.2 | `Id`: 1–255 characters of `[A-Za-z0-9_-]`, checked on construction and deserialization |
| 8620 | §2 | `Session`, `Account`, `CoreCapability` (`Session::core()`) |
| 8620 | §3.2–3.4 | `Invocation` (`[name, arguments, callId]`), `Request`, `Response` |
| 8620 | §3.6 | `ProblemDetails` (request level), `MethodError` (`error` response, `Invocation::error`) |
| 8620 | §3.7 | `ResultReference` (type only; resolution is the server's) |
| 8620 | §5.1 | `GetRequest`, `GetResponse<T>` |
| 8620 | §5.3 | `SetRequest<T>` (updates as PatchObject maps), `SetResponse<T>`, `SetError` |
| 8620 | §5.5 | `QueryRequest<F>` (defaults: `position` 0, `anchorOffset` 0, `calculateTotal` false), `Filter<F>`, `Comparator` (`isAscending` default true), `QueryResponse` |
| 8621 | §2 | `Mailbox`, `MailboxRights` |
| 8621 | §3 | `Thread` |
| 8621 | §4.1 | `Email` (every property optional), `EmailAddress`, `EmailBodyPart`, `EmailBodyValue` |
| 8621 | §4.4.1 | `EmailFilterCondition` |

Not covered: `/changes`, `/copy`, `/queryChanges`, push, blob upload and
download, `EmailSubmission`, `Identity`, header parsed forms as typed fields.

## Extra properties

`Email`, `EmailBodyPart`, `EmailFilterCondition`, `Mailbox`, `GetRequest` and
`MethodError` keep properties they do not name in `extra`, so a round trip
loses nothing: vendor properties (`noolog:blog`) and header forms
(`header:List-Post:asURLs`).

A server shows a vendor property only to a client that listed its capability
in `using` (RFC 8620 §1.8). This crate does not enforce that; the code mapping
domain objects to these types does.

## Conformance

| Check | Against |
|---|---|
| Round trip | RFC 8620 §2.1, §3.3.1, §3.4.1; RFC 8621 §3.1.1, §4.2.1. The §2.1 Session is repaired first: a missing comma, `...` placeholders and wrapped URLs make the printed text invalid JSON |
| Independent parser | `jmap-client` 0.4.3 parses serialized `Email`, `Mailbox` and `Thread/get` responses (no vendor properties: it reads any unknown `name:form` property as a header form) |
