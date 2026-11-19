# Product Vision: Reno

Status: Draft
Date: 2026-11-19

## Problem

Coding harnesses (Claude Code, Codex, Pi, opencode, Cursor and ACP agents) each run alone, with their own approvals, sessions and limits. Nothing coordinates who does what, when, in which workspace, or whether the result was good enough.

## Users

The owner: a developer who runs several coding agents on their own Mac and wants them scheduled, bounded, approved and reviewed from one place.

## Goals

None yet — set with `/define-project` (use `**GOAL-N**` IDs).

## Non-goals

None yet — set with `/define-project`. Known from the architecture: Reno does not run a model loop of its own.

## Scope boundaries

Local-first and macOS. Reno drives existing harness CLIs and never writes their config, auth or Keychain entries.
