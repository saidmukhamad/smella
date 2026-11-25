import { save } from './index.js';
import type { Options, factory } from './types.js';
import { unread } from './ordinary.js';
import './install.js';
const options: Options = {};
let result: ReturnType<typeof factory>;
save();
