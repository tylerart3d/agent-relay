import { host } from '@hermes/plugin-sdk'

const FLEET_URL = 'http://127.0.0.1:38475/agent-relay/hermes'
const POLL_MS = 750
const DRAFT_TIMEOUT_MS = 2500
const VERIFY_SETTLE_MS = 75

const delay = milliseconds => new Promise(resolve => window.setTimeout(resolve, milliseconds))

export default {
  id: 'agent-relay',
  name: 'Agent Relay',
  description: 'Starts a fresh Hermes chat on the model selected from Agent Relay.',
  register(ctx) {
    const clientId = crypto.randomUUID()
    let disposed = false
    let focusedAt = document.hasFocus() ? Date.now() : 0
    let lastHandledRevision = 0
    let polling = false
    let pendingAck = null

    const markFocused = () => {
      focusedAt = Date.now()
    }

    const paintModel = model => {
      // Hermes exposes this atom as readonly in the SDK type contract, but the
      // runtime object is the live nanostore used by its own picker. The RPC
      // changes the backend session; painting this atom keeps the composer pill
      // in sync with that already-authoritative session change.
      if (typeof host.state.model.set !== 'function') {
        throw new Error('this Hermes build does not expose a writable model state')
      }
      host.state.model.set(model)
      localStorage.setItem('hermes.desktop.composer.model', model)
    }

    const acknowledge = async (command, result, error) => {
      const response = await fetch(`${FLEET_URL}/ack`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          client_id: clientId,
          revision: command.revision,
          session_id: command.session_id,
          state: error ? 'error' : result?.deferred ? 'deferred' : 'switched',
          deferred: Boolean(result?.deferred),
          error: error ? String(error) : null
        })
      })
      // A conflict means this command expired or was superseded before the ACK
      // arrived. It is terminal for this client, but not a successful delivery.
      if (response.status === 409) return false
      if (!response.ok) throw new Error(`Agent Relay ACK failed with HTTP ${response.status}`)
      return true
    }

    const waitForFreshDraft = async () => {
      const deadline = Date.now() + DRAFT_TIMEOUT_MS
      while (Date.now() < deadline) {
        if (host.state.activeSessionId.get() === null) return true
        await delay(VERIFY_SETTLE_MS)
      }
      return false
    }

    const switchFreshDraft = async model => {
      const previousModel = host.state.model.get()
      // Route navigation is the only SDK-supported way for a disk plugin to
      // request the new-session surface. The custom event is just Hermes's
      // standard keyboard-affordance flash; state below is the authority.
      host.navigate('/')
      window.dispatchEvent(new CustomEvent('hermes:new-session-shortcut'))
      if (!(await waitForFreshDraft())) {
        return { deferred: true, message: 'Hermes did not expose a fresh draft in time.' }
      }

      paintModel(model)
      await delay(VERIFY_SETTLE_MS)
      const firstVerified =
        host.state.activeSessionId.get() === null && host.state.model.get() === model
      await delay(VERIFY_SETTLE_MS)
      const stableVerified =
        host.state.activeSessionId.get() === null && host.state.model.get() === model
      if (!firstVerified || !stableVerified) {
        try {
          paintModel(previousModel)
        } catch {}
        return {
          deferred: true,
          message: 'Hermes did not retain the selected model on a fresh draft.'
        }
      }
      return { deferred: false }
    }

    const deliverPendingAck = async () => {
      if (!pendingAck) return true
      const { command, result, error } = pendingAck
      const accepted = await acknowledge(command, result, error)
      lastHandledRevision = command.revision
      pendingAck = null
      return accepted
    }

    const poll = async () => {
      if (disposed || polling) return
      polling = true

      try {
        // Retry transport delivery without applying the model switch twice.
        if (pendingAck) {
          await deliverPendingAck()
          return
        }
        const sessionId = host.state.activeSessionId.get()
        const response = await fetch(`${FLEET_URL}/presence`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            client_id: clientId,
            session_id: sessionId,
            visible_model: host.state.model.get(),
            focused_at_ms: focusedAt,
            last_handled_revision: lastHandledRevision
          })
        })
        if (!response.ok) return

        const { command } = await response.json()
        if (!command || command.revision <= lastHandledRevision) return

        let result = null
        let error = null
        try {
          // A Hermes chat owns the model it started with. Agent Relay updates the
          // profile default before publishing this command, then opens a fresh
          // draft and paints that default into the composer state. The first
          // send creates the new session with this model; the prior chat is
          // preserved unchanged in history.
          result = await switchFreshDraft(command.model)
          if (result.deferred) {
            host.notify({
              kind: 'warning',
              title: 'Agent Relay model switch deferred',
              message: result.message
            })
          }
        } catch (caught) {
          error = caught instanceof Error ? caught.message : String(caught)
          host.notify({
            kind: 'error',
            title: 'Agent Relay model switch failed',
            message: error
          })
        }

        pendingAck = { command, result, error }
        await deliverPendingAck()
      } catch {
        // Agent Relay may be stopped; reconnect silently on the next heartbeat.
      } finally {
        polling = false
      }
    }

    window.addEventListener('focus', markFocused)
    const timer = window.setInterval(() => void poll(), POLL_MS)
    void poll()

    ctx.onDispose(() => {
      disposed = true
      window.clearInterval(timer)
      window.removeEventListener('focus', markFocused)
    })
  }
}
