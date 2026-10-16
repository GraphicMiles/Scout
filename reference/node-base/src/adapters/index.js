import { ScoutError } from '../errors.js';
import { genericPageStrategy, selectorsStrategy } from '../extract.js';
import { youtubeChannelVideos } from './youtube.js';

const notImplemented = (name, description, reason) => ({
  name, description, status: 'not_implemented', reason,
  run() { throw new ScoutError('unsupported_capability', reason); },
});

export function buildStrategies(cfg) {
  const list = [
    genericPageStrategy(),
    selectorsStrategy(),
    youtubeChannelVideos(cfg),
    notImplemented('facebook-jobs', 'Public job posts.',
      'Facebook adapters are not implemented. Pages are rendered with JavaScript and mostly require sign-in; Scout core has no JavaScript runtime yet and does not bypass access controls. Verify against real pages before adding.'),
    notImplemented('facebook-marketplace', 'Public marketplace listings.',
      'Facebook adapters are not implemented. Marketplace requires JavaScript rendering and usually authentication; Scout does not bypass access controls.'),
    notImplemented('tiktok-profile', 'Public profiles and post descriptions.',
      'TikTok adapter is not implemented. It needs verification against real pages and likely a JavaScript runtime; Scout does not bypass anti-bot protections.'),
  ];
  return new Map(list.map((s) => [s.name, s]));
}
