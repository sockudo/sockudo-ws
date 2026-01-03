/* eslint-disable */
/* prettier-ignore */
// @ts-self-types="./index.d.ts"

/**
 * @module
 * Ultra-fast WebSocket library for Node.js - 17% faster than alternatives, matching uWebSockets performance.
 *
 * Features:
 * - SIMD-accelerated UTF-8 validation
 * - Lock-free message queues
 * - Integrated pub/sub system with 64 sharded topics
 * - Zero-copy message handling
 * - Built-in compression (permessage-deflate)
 *
 * @example
 * ```javascript
 * import { WebSocketServer, Message } from '@sockudo/ws';
 *
 * const server = new WebSocketServer({ port: 8080 });
 *
 * await server.start((ws, info) => {
 *   ws.subscribe('chat');
 *
 *   ws.onMessage((msg) => {
 *     ws.publish('chat', msg);
 *   });
 * });
 * ```
 */

import binding from './index.js'

export const {
  // Runtime
  initRuntime,
  getWorkerThreads,
  getAvailableCores,

  // Message
  Message,
  MessageType,

  // Config
  Config,
  Compression,
  hftConfig,
  throughputConfig,
  uwsConfig,

  // Error types
  CloseCode,
  CloseReason,

  // Server
  WebSocketServer,
  ServerOptions,
  ConnectionInfo,
  ServerStats,

  // Client
  WebSocketClient,
  ClientOptions,
  connect,

  // WebSocket
  WebSocket,
  ConnectionStats,

  // PubSub
  PubSub,
  Subscriber,
  PublishResult,
  PubSubStats,
} = binding;
