import { describe, it, expect, afterEach } from 'vitest'
import WebSocket from 'ws'
import {
  createTestGatewayServer,
  seedBot,
  seedGuild,
  seedChannel,
} from '../test-helpers'
import { createCommand } from '../services/application-commands'
import { getOrCreateDmChannel } from '../services/channels'
import { createTestUser } from '../services/test-control'
import { GatewayOp } from './opcodes'

// Fields real Discord always sends and strict clients (twilight-model) require.
const INTENTS = 1 | 512 | 4096 | 32_768 // GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT
const APP_ID = '111111111111111111'
const TOKEN = 'Bot fidelity-token'

type Event = Record<string, unknown> & { t?: string; d?: unknown }

describe('native payload fidelity', () => {
  let close: (() => Promise<void>) | undefined
  let ws: WebSocket | undefined

  afterEach(async () => {
    ws?.terminate()
    ws = undefined
    await close?.()
    close = undefined
  })

  async function start() {
    const server = await createTestGatewayServer()
    close = server.close
    const bot = seedBot(server.db, TOKEN, APP_ID)
    const guild = seedGuild(server.db, bot)
    const channel = seedChannel(server.db, guild)
    const user = createTestUser(server.db, { username: 'Human' })
    server.db
      .prepare('INSERT INTO guild_members (guild_id, user_id) VALUES (?, ?)')
      .run(guild, user.id)
    const http = server.url.replace('ws://', 'http://')

    const events: Event[] = []
    const waiters: { t: string; resolve: (e: Event) => void }[] = []
    ws = new WebSocket(server.url)
    await new Promise((resolve) => ws?.once('message', resolve))
    ws.on('message', (raw: Buffer) => {
      const event = JSON.parse(raw.toString()) as Event
      const i = waiters.findIndex((w) => w.t === event.t)
      if (i === -1) {
        events.push(event)
      } else {
        waiters.splice(i, 1)[0].resolve(event)
      }
    })
    ws.send(
      JSON.stringify({
        op: GatewayOp.Identify,
        d: { token: TOKEN, intents: INTENTS },
      })
    )
    const next = (t: string) =>
      new Promise<Event>((resolve) => {
        const seen = events.findIndex((e) => e.t === t)
        if (seen === -1) {
          waiters.push({ t, resolve })
        } else {
          resolve(events.splice(seen, 1)[0])
        }
      })
    await next('READY')
    const post = (path: string, body: unknown, auth = false) =>
      fetch(`${http}${path}`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          ...(auth && { Authorization: TOKEN }),
        },
        body: JSON.stringify(body),
      })
    const patch = (path: string, body: unknown) =>
      fetch(`${http}${path}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json', Authorization: TOKEN },
        body: JSON.stringify(body),
      })
    return { db: server.db, guild, channel, user, next, post, patch }
  }

  it('resolves user and role mentions from injected human content', async () => {
    const { channel, user, next, post } = await start()
    const res = await post(`/_test/channels/${channel}/messages`, {
      content: `<@${APP_ID}> hi <@!${APP_ID}> <@999> <@&222>`,
      author: { id: user.id },
    })
    expect(res.status).toBe(201)
    const message = (await res.json()) as {
      mentions: { id: string; bot: boolean }[]
      mention_roles: string[]
    }
    // Deduplicated, unknown users dropped, roles collected.
    expect(message.mentions.map((m) => m.id)).toEqual([APP_ID])
    expect(message.mentions[0].bot).toBe(true)
    expect(message.mention_roles).toEqual(['222'])

    const event = await next('MESSAGE_CREATE')
    expect(event.d).toMatchObject({
      mentions: [{ id: APP_ID }],
      mention_roles: ['222'],
    })
  })

  it('dispatches THREAD_CREATE when the bot opens a thread', async () => {
    const { channel, next, post } = await start()
    const res = await post(
      `/api/v10/channels/${channel}/threads`,
      { name: 'Bee · Human', type: 11 },
      true
    )
    expect(res.status).toBe(201)
    const thread = (await res.json()) as { id: string }
    const event = await next('THREAD_CREATE')
    expect(event.d).toMatchObject({
      id: thread.id,
      parent_id: channel,
      newly_created: true,
    })
  })

  it.each([true, false])(
    'sends authorizing_integration_owners, context and entitlements (in guild: %s)',
    async (inGuild) => {
      const { db, guild, channel, user, next, post } = await start()
      createCommand(db, APP_ID, null, { name: 'new', description: 'New' })
      const dm = getOrCreateDmChannel(db, APP_ID, user.id).id
      const res = await post('/_test/interactions', {
        application_id: APP_ID,
        command_name: 'new',
        guild_id: inGuild ? guild : undefined,
        channel_id: inGuild ? channel : dm,
        user_id: user.id,
      })
      expect(res.status).toBe(201)
      const event = await next('INTERACTION_CREATE')
      expect(event.d).toMatchObject({
        authorizing_integration_owners: inGuild
          ? { '0': guild }
          : { '1': user.id },
        context: inGuild ? 0 : 1,
        entitlements: [],
        app_permissions: '0',
      })
    }
  )

  it('keeps the buttons a bot sends, and drops them when edited away', async () => {
    const { channel, next, post, patch } = await start()
    const components = [
      {
        type: 1,
        components: [
          { type: 2, style: 1, label: 'Approve', custom_id: 'desk:approve:1' },
        ],
      },
    ]
    const res = await post(
      `/api/v10/channels/${channel}/messages`,
      { content: 'Decide', components },
      true
    )
    expect(res.status).toBe(200)
    const sent = (await res.json()) as { id: string; components: unknown[] }
    expect(sent.components).toEqual(components)
    const event = await next('MESSAGE_CREATE')
    expect(event.d).toMatchObject({ id: sent.id, components })

    const edited = await patch(
      `/api/v10/channels/${channel}/messages/${sent.id}`,
      { content: 'Decided', components: [] }
    )
    expect(edited.status).toBe(200)
    expect(await edited.json()).toMatchObject({ components: [] })
  })

  it('simulates a button press with its custom_id', async () => {
    const { guild, channel, user, next, post } = await start()
    const res = await post('/_test/interactions', {
      application_id: APP_ID,
      type: 3,
      custom_id: 'access:approve:42',
      guild_id: guild,
      channel_id: channel,
      user_id: user.id,
    })
    expect(res.status).toBe(201)
    const event = await next('INTERACTION_CREATE')
    expect(event.d).toMatchObject({
      type: 3,
      data: { custom_id: 'access:approve:42', component_type: 2 },
      member: { user: { id: user.id } },
    })

    const missing = await post('/_test/interactions', {
      application_id: APP_ID,
      type: 3,
      channel_id: channel,
    })
    expect(missing.status).toBe(404)
  })
})
