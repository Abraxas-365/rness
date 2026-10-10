import mark from '../assets/rness-mark.svg?raw'
import type { APIRoute } from 'astro'

// Serve the brand mark rather than maintaining another copy.
export const GET: APIRoute = () => new Response(
  mark,
  { headers: { 'Content-Type': 'image/svg+xml' } },
)
