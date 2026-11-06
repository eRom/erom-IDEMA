# Sécurité

## Signaler une vulnérabilité

Ne publiez pas une vulnérabilité dans une issue publique. Utilisez le signalement privé de GitHub : onglet **Security** du dépôt, puis **Report a vulnerability**. Décrivez la version d'IdemA (`idema --version`), la version de macOS et les étapes pour reproduire. Le signalement est traité en priorité, et vous êtes tenu au courant jusqu'au correctif.

Seule la dernière version publiée reçoit des correctifs de sécurité.

## Ce qu'IdemA fait de vos données

IdemA n'a ni télémétrie, ni statistiques d'usage, ni compte.

### Réseau

Au lancement, pour la ligne IdnmA du menu Aide, IdemA lance `idnma --version` dans un shell de connexion (local ; il charge votre configuration de shell) et un `git ls-remote` vers le dépôt public d'IdnmA, la seule requête réseau partie sans geste de votre part.

Tout le reste part d'un geste :

| Geste | Ce qui sort | Vers |
|---|---|---|
| Générer un message de commit (option éteinte par défaut) | le diff indexé | l'API Gemini de Google, avec votre clé |
| Installer le réseau IDNMA (menu IdemA) | téléchargement puis exécution d'un script d'installation | `erom.cloud` |
| Chercher ou ajouter une compétence (volet Projet) | la recherche, le paquet `skills` | skills.sh et le registre npm, via `bunx skills` |
| Ouvrir la galerie Artifacts | une requête avec votre session claude.ai | claude.ai |
| Naviguer dans le volet Browser, onglets rouverts compris | ce qu'envoie n'importe quel navigateur | les sites visités |
| Notification du compagnon (éteint par défaut) | le nom de la session et ses dernières lignes, chiffrés pour l'appareil appairé | le service push de l'appareil, `web.push.apple.com` pour un iPhone |

### Secrets

- La clé de l'API du message de commit est stockée en clair dans `~/.config/idema/config.toml`. Elle n'est jamais passée en argument d'un processus (elle transite par l'entrée standard de `curl`) et elle est masquée partout où IdemA l'affiche ou la journalise. Restreignez le fichier : `chmod 600 ~/.config/idema/config.toml`.
- Les cookies du navigateur intégré restent dans le magasin WebKit d'IdemA. IdemA n'en lit jamais la valeur.
- Le journal `~/.config/idema/logs/idema.log` ne contient ni clé, ni texte recherché, ni variables d'environnement des commandes Git.
- Les terminaux démarrent dans un environnement nettoyé : les variables de session d'un agent qui aurait lancé IdemA n'y passent pas.
- Compagnon iPhone (éteint par défaut) : le jeton d'un appareil appairé n'est gardé que sous forme d'empreinte, et son cookie est borné au chemin `/idema-api`. Le jeton d'un appareil vaut un accès en écriture aux panes : qui le détient peut taper dans vos sessions. Oubliez l'appareil dans Personnaliser › Compagnon pour le révoquer.
- La clé VAPID du compagnon est écrite en mode 0600 dans le dossier de configuration d'IdemA. Le service n'écoute que sur `127.0.0.1` ; l'exposer sur votre tailnet (`tailscale serve`) est votre choix, et Funnel (exposition sur Internet) est déconseillé.
- Défense contre les requêtes intersites : aucune route d'écriture du compagnon n'accepte autre chose que du JSON, et le service n'envoie jamais d'en-tête CORS. Ces deux règles ne se relâchent pas.
- Le compagnon n'envoie aucune télémétrie : les mesures de la page ne sont écrites que dans le journal local.

### Git

Le volet Changes ne lance que des commandes Git d'une liste blanche, et aucune commande destructive : ni `reset`, ni `checkout`, ni `clean`, ni push forcé.

### Distribution

Le DMG est construit par GitHub Actions à partir du tag de la release, signé ad hoc et non notarisé par Apple. Chaque release publie l'empreinte SHA-256 du DMG.
