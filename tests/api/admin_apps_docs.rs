//! The admin OAuth-app maintenance and instance-document endpoints over
//! HTTP (WBE-205): `GET`/`POST /api/v1/pleroma/admin/oauth_app`,
//! `PATCH`/`DELETE …/oauth_app/{id}` — the filters, the window, the
//! seven-key entity, the `can't be blank` refusals and the `trusted`
//! flag — and the `instance_document/{name}` trio with the public JSON
//! `GET /api/v1/instance/terms_of_service`, its 1 MiB cap, its atomic
//! store and the traversal gate, against the real router and a document
//! directory in `tempfile`. The auth half covers both areas'
//! `admin:read` / `admin:write` scope gates.

#![cfg(test)]

mod support;
