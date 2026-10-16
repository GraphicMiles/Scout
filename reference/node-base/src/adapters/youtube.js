import { ScoutError } from '../errors.js';
import { querySelectorAll } from '../selector.js';
import { rawText } from '../html.js';
import { extractJsonAfter, walkJson } from '../util.js';

const ytText = (t) => t?.simpleText ?? (Array.isArray(t?.runs) ? t.runs.map((r) => r.text).join('') : null) ?? null;

/** Parse a YouTube channel "Videos" page already loaded into a DOM. Exported for fixture tests. */
export function parseYoutubeChannel(doc, { limit = 20 } = {}) {
  let initial = null;
  for (const s of querySelectorAll(doc, 'script')) {
    const body = rawText(s);
    if (!body.includes('ytInitialData')) continue;
    const json = extractJsonAfter(body, 'ytInitialData');
    if (json) { try { initial = JSON.parse(json); break; } catch { /* try next script */ } }
  }
  if (!initial) {
    throw new ScoutError('compatibility_error', 'YouTube did not return embedded page data (ytInitialData). The page likely requires JavaScript, a consent step, or blocked this client; Scout does not bypass these.');
  }
  let channel = null;
  const videos = []; const seen = new Set();
  for (const o of walkJson(initial)) {
    if (!channel && o.channelMetadataRenderer) {
      const c = o.channelMetadataRenderer;
      channel = {
        id: c.externalId ?? null, title: c.title ?? null, url: c.channelUrl ?? null,
        handle: /@[\w.-]+/.exec(c.vanityChannelUrl ?? '')?.[0] ?? null, description: c.description ?? null,
      };
    }
    const r = o.videoRenderer ?? o.gridVideoRenderer;
    if (r?.videoId && !seen.has(r.videoId)) {
      seen.add(r.videoId);
      videos.push({
        videoId: r.videoId, title: ytText(r.title), url: `https://www.youtube.com/watch?v=${r.videoId}`,
        publishedText: ytText(r.publishedTimeText), viewCountText: ytText(r.viewCountText), duration: ytText(r.lengthText),
      });
    }
  }
  const warnings = [];
  if (!channel) warnings.push({ code: 'channel_missing', message: 'Channel metadata was not present in the page data' });
  if (!videos.length) warnings.push({ code: 'no_videos', message: 'No videos found; the layout may have changed or the channel has no public videos' });
  else if (videos.length < limit) warnings.push({ code: 'continuation_unsupported', message: `Found ${videos.length} videos; loading more requires continuation requests that Scout core does not perform` });
  return { data: { channel, videos: videos.slice(0, limit) }, warnings };
}

export function youtubeChannelVideos(cfg) {
  const hosts = () => cfg.youtubeHosts;
  return {
    name: 'youtube-channel-videos', status: 'fixture-verified-only', defaultLimit: 20, embeddedData: true,
    description: 'Channel identity and recent videos (title, id, URL, visible published text) from a channel Videos page.',
    prepareUrl(raw) {
      let u;
      try { u = new URL(raw); } catch { throw new ScoutError('invalid_request', 'URL is not valid', { details: { field: 'url' } }); }
      if (!hosts().includes(u.hostname.toLowerCase())) throw new ScoutError('invalid_request', 'This strategy requires a youtube.com channel URL', { details: { field: 'url' } });
      if (/^\/(@[^/]+|channel\/[^/]+|c\/[^/]+|user\/[^/]+)\/?$/.test(u.pathname)) u.pathname = u.pathname.replace(/\/$/, '') + '/videos';
      return u.href;
    },
    run({ page, limit }) {
      const u = new URL(page.url);
      if (u.hostname === 'consent.youtube.com' || u.pathname.startsWith('/consent')) {
        throw new ScoutError('compatibility_error', 'YouTube served a consent interstitial. Scout does not accept consent flows on the caller\'s behalf.');
      }
      if (page.kind !== 'html') throw new ScoutError('compatibility_error', 'Expected an HTML page');
      return parseYoutubeChannel(page.doc, { limit });
    },
  };
}
