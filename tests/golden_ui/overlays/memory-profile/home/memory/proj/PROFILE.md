---
alias: proj
generated_at: 2026-01-10T08:00:00Z
head_sha: 0123456789abcdef0123456789abcdef01234567
status: approved
---

<!-- section: overview origin: agent verified: 2026-01-10T08:00:00Z summary: A Rails storefront that sells event tickets & passes; used by the box office and customers. -->
## Overview

The storefront for event tickets. Customers buy passes; the box office prints them.

<!-- section: structure origin: agent verified: 2026-01-10T08:00:00Z summary: Standard Rails layout; checkout lives in app/services/checkout. -->
## Structure

- `app/models`: tickets, passes, orders
- `app/services/checkout`: the purchase flow

### Sources
- `config/routes.rb` sha256:1111111111111111111111111111111111111111111111111111111111111111

<!-- section: features origin: agent verified: 2026-01-10T08:00:00Z summary: Ticket checkout, season passes and refunds. -->
## Features

### Checkout

The **checkout** flow lives in `app/services/checkout` and talks to the payments API.

### Refunds

Refunds are issued from the admin, see `app/admin/refunds.rb`.

### Sources
- `app/services/checkout/flow.rb` sha256:2222222222222222222222222222222222222222222222222222222222222222
- `app/admin/refunds.rb` sha256:3333333333333333333333333333333333333333333333333333333333333333

<!-- section: stack origin: agent verified: 2026-01-10T08:00:00Z summary: Ruby 3.3, Rails 7.1, PostgreSQL 16 and Redis. -->
## Stack

Ruby 3.3 and Rails 7.1 on PostgreSQL 16; Sidekiq on Redis for jobs.

<!-- section: integrations origin: agent verified: 2026-01-10T08:00:00Z summary: Calls the other service for seat maps; Stripe for payments. -->
## Integrations

- `other`: seat maps over HTTP
- `stripe`: payments

<!-- section: secrets origin: agent verified: 2026-01-10T08:00:00Z summary: Two secrets, both from the team vault. -->
## Secrets

- `STRIPE_API_KEY`: env var, from the team vault, rotated by ops
- `SEATMAP_TOKEN`: credentials file, from the other team

<!-- section: conventions origin: human verified: 2026-01-12T09:00:00Z summary: Branch per issue; run bin/rspec and rubocop . before a merge request. -->
## Conventions

Branch per issue. Run `bin/rspec` and `bundle exec rubocop .` before opening a merge request.

<!-- section: gotchas origin: agent verified: 2026-01-10T08:00:00Z summary: Run the migration before the specs. -->
## Gotchas

Run the migration before the specs; `config/database.yml` is not tracked.

<!-- links -->
## Links

- `other` | calls | config/services.yml:12
