---
id: fetcher-goals
type: goal-and-requirements
status: active
title: Fetcher goals and requirements
---

# Fetcher

## Goal and audience

Turn supported social-media links into media delivered directly in Telegram, without requiring the recipient to run download tools. Fetcher is a self-hosted convenience bot for its operator and trusted chats, not a public multi-tenant download service (audience confirmed by the user). This describes intended use, not an enforced access restriction.

## Capabilities

- Download Instagram Reels and the photos in single-image or carousel posts; video covers are not substitutes for requested photos.
- Offer Instagram profile downloads of Reels, photos, or both. Profile Reels become one video with circle transitions; photos are delivered in batches.
- Download X status videos without cookies.
- Offer YouTube video or MP3 audio, including Shorts and live-link forms. Offer available native manual or automatic captions as separate SRT documents, independently of media downloads.
- Reply to inline queries immediately with guidance to download in bot chat; do not run downloads inside inline-query handling.
- Show queue/progress feedback, prepare Telegram-compatible video, and support cloud or operator-configured local Bot API uploads with size-aware delivery.
- Optionally copy existing Firefox sessions through a private cookie bridge, retaining usable local cookies when refresh fails.

## Durable principles

Keep source links in delivered media captions. Treat unavailable media, extraction failures, and Telegram delivery errors as reportable outcomes rather than promising every link will work.

Request logging and optional error alerts exist for operational troubleshooting (purpose confirmed by the user), not as an asserted analytics product. Their implementation and privacy limits belong to `fetcher-bot`.

## Scope boundaries

The cookie bridge copies sessions; it neither signs in nor renews expired platform sessions. Its private transport and credential-handling contract belongs to `fetcher-cookie-bridge`.

**Unconfirmed scope inference:** the shipped Telegram bot and cookie bridge define the product documented here; the local Android share implementation plan is input, not an implemented capability or an adopted requirement. No broader exclusion of companion clients or additional platforms is established.

## Acknowledged constraints

The user confirmed that pending work and menus are in memory and are lost on restart, and that resource bounds are partial. These are current limitations, not promises to preserve them; their precise behavior belongs to `fetcher-bot`.

## Spec map

`fetcher-architecture` owns topology, dependency edges, and deployment assumptions. `fetcher-bot` and `fetcher-cookie-bridge` own their respective runtime contracts. Unconfirmed statements remain explicitly marked; approval of the graph does not independently verify them.
