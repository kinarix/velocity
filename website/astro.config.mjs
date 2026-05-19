import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://velocity.kinarix.com',
  integrations: [
    starlight({
      title: 'Velocity',
      description: 'Schema-driven, Kubernetes-native backend platform',
      social: {
        github: 'https://github.com/kinarix/velocity',
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Home', link: '/' },
            { label: 'Getting Started', link: '/getting-started/' },
            { label: 'Installation', link: '/installation/' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Hardening', link: '/hardening/' },
            { label: 'Security', link: '/security/' },
            { label: 'Troubleshooting', link: '/troubleshooting/' },
            { label: 'API Reference', link: '/api-reference/' },
          ],
        },
        {
          label: 'Architecture',
          items: [
            { label: 'Overview', link: '/architecture/' },
            { label: 'Database schema', link: '/architecture/database/' },
            { label: 'Stored procedures', link: '/architecture/stored-procedures/' },
            { label: 'RLS and grants', link: '/architecture/rls-and-grants/' },
            { label: 'Migrations', link: '/architecture/migrations/' },
          ],
        },
        {
          label: 'Features',
          items: [
            { label: 'Schema Definition', link: '/features/schema-definition/' },
            { label: 'Authentication', link: '/features/auth/' },
            { label: 'Time Machine', link: '/features/time-machine/' },
            { label: 'Archive & Purge', link: '/features/archive/' },
            { label: 'Audit', link: '/features/audit/' },
            { label: 'Search', link: '/features/search/' },
            { label: 'Observability', link: '/features/observability/' },
            { label: 'CLI', link: '/features/cli/' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Changelog', link: '/changelog/' },
            { label: 'Architecture Decisions', link: '/adrs/' },
          ],
        },
        {
          label: 'Runbooks',
          items: [
            { label: 'Postgres Failover', link: '/runbooks/postgres-failover/' },
            { label: 'Restore from Backup', link: '/runbooks/restore-from-backup/' },
            { label: 'Rotate API Key', link: '/runbooks/rotate-api-key/' },
            { label: 'Quarantine Drifted Schema', link: '/runbooks/quarantine-drifted-schema/' },
            { label: 'On-Call Cheatsheet', link: '/runbooks/oncall-cheatsheet/' },
          ],
        },
      ],
    }),
  ],
});
