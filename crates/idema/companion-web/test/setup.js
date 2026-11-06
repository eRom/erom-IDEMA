// bun test n'a pas de DOM : happy-dom fournit document, localStorage, EventSource et le reste.
import { GlobalRegistrator } from '@happy-dom/global-registrator';

GlobalRegistrator.register({ url: 'http://localhost:3000/' });
