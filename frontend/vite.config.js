import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react(), {
    name: 'local-demo-config',
    configureServer(server) {
      server.middlewares.use('/__local/config', (request, response) => {
        const local = ['127.0.0.1', '::1', '::ffff:127.0.0.1'].includes(request.socket.remoteAddress);
        if (!local || !process.env.LOCAL_DEMO_CONFIG) { response.statusCode = 404; response.end(); return; }
        response.setHeader('Content-Type', 'application/json');
        response.setHeader('Cache-Control', 'no-store');
        response.end(process.env.LOCAL_DEMO_CONFIG);
      });
    },
  }],
});
