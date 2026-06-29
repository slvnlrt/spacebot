# Auth du portail admin & posture de déploiement — constat de sécurité

> Date : 2026-06-23. **Constat documenté** (décision : tracer le gap, agir plus tard).
> Vérifié dans le code, pas supposé. Concerne la **console d'administration** (UI web + API `/api/*`).

## ⚠️ Périmètre — à ne pas confondre

Ce document parle de l'**authentification des opérateurs/admins** sur le **portail de gestion** de spacebot
(le dashboard web et l'API qui le sert). C'est **distinct** de :

- **L'identité des utilisateurs finaux** (les humains qui discutent avec l'agent via Discord/Slack/etc.) et du
  **user-scoping des mémoires** (suivi ailleurs comme « I7 » dans les docs mémoire). Ce sont les gens *en contact
  avec l'agent par les canaux de chat* — rien à voir avec qui *administre* l'instance.

Les deux sont des chantiers séparés. Ce doc = **accès à la console admin uniquement**.

## Constat (vérifié dans le code)

Le serveur API/UI (`src/api/server.rs`) :

- **Auth = un seul token Bearer statique partagé**, lu depuis `config.api.auth_token` (`Option<String>`,
  `src/config/toml_schema.rs:100`). Pas de login, pas de sessions, pas de SSO/OIDC, pas de comptes.
- **Opt-in, OFF par défaut** : si `auth_token` n'est pas configuré → `None` → le middleware
  **laisse toutes les requêtes passer sans auth** (`src/api/server.rs:352` : `let Some(expected_token) = … else { return next.run(request).await }`). **Aucune auto-génération, aucun warning.**
- **Token = admin total** : qui l'a peut tout faire, **y compris l'API secrets** (`/api/secrets/*`). Aucune
  autorisation/RBAC, aucun cloisonnement par opérateur.
- **Frontend servi sans auth** : les fichiers statiques du dashboard sont le `fallback` non protégé
  (`server.rs:323-324`) ; seules les requêtes `/api/*` sont gardées (quand un token est posé). `/api/health` et
  `/health` sont toujours publics. Le navigateur envoie le token depuis `localStorage["spacebot_auth_token"]`
  (`interface/src/api/client-typed.ts:18`) — pas d'écran de login dédié.
- **Pas de TLS** : HTTP en clair → le token Bearer circule en clair sur le réseau.
- **Adresse d'écoute** (`src/config/toml_schema.rs:117-156`, `types.rs:173/195`) :
  - binaire natif : `api.bind` par défaut = **`127.0.0.1`** (localhost only), port **19898** → injoignable depuis le LAN ;
  - déploiement Docker : bind = **`0.0.0.0`** (toutes interfaces) → **exposé sur le réseau**.

### Le footgun principal

`bind = 0.0.0.0` **+** `auth_token` non défini = **console d'admin grande ouverte sur le réseau, silencieusement**
(aucun warning, fail-open). C'est le scénario à éviter absolument sur un réseau d'entreprise partagé.

## Évaluation enterprise-readiness

C'est un design **local-first / mono-opérateur** assumé — correct pour un déploiement personnel derrière un tunnel,
**insuffisant pour du multi-admin sur un réseau partagé**. Manquent, pour de l'enterprise :

| Dimension | Actuel | Cible enterprise |
|---|---|---|
| Authentification | 1 token statique, opt-in | Login + **SSO/OIDC** (Google/Okta/Entra), sessions |
| Autorisation | aucune (token = admin total) | **RBAC** opérateurs, scoping des actions |
| Transport | HTTP clair | **TLS** |
| Audit | aucun | journal « qui a fait quoi, quand » |
| Défaut | fail-open (0.0.0.0 sans token) | fail-closed |

## Posture de déploiement recommandée (sans changement de code)

**1. Mono-opérateur / test (recommandé immédiat) :** garder `bind = 127.0.0.1` + accès par **tunnel SSH**
(`ssh -L 19898:127.0.0.1:19898 <hôte>`). Zéro exposition réseau, rien d'autre à configurer.

**2. Multi-admin / réseau d'entreprise (sans code, enterprise-grade) :** binder spacebot sur `127.0.0.1` et mettre
**un reverse proxy devant** :
- **Caddy / nginx** pour le **TLS** ;
- **oauth2-proxy** ou **Authelia** en *forward-auth* pour le **SSO + login par opérateur + audit d'accès**.
- C'est le pattern standard pour les outils internes sans auth applicative ; légitime et enterprise-déployable.

**3. Si on doit binder sur le LAN sans proxy :** au minimum, **définir `[api] auth_token = "<secret long aléatoire>"`**
+ firewall restreint aux IP autorisées. ⚠️ vérifier qu'un appel `/api/...` sans header renvoie bien **401**. (Reste
sans TLS → token en clair : déconseillé sur réseau partagé.)

## ⚠️ Bug applicatif : l'auth-ON casse les chargements natifs du navigateur (vérifié 2026-06-29)

**Constat (app-wide, pas spécifique à une feature) :** quand `auth_token` **est défini**, `api_auth_middleware`
(`src/api/server.rs:362-367`) exige un header `Authorization: Bearer` sur tout `/api/*` (sauf `/health`). Or ce token
ne vit que dans le `localStorage` et n'est attaché **que par le client JS `fetch`** (`interface/src/api/client-typed.ts:18`).
Donc **tout ce que le navigateur charge nativement — sans passer par `fetch` — part sans token et reçoit un 401** :

- **Téléchargements** via `<a href download>` : pièces jointes (`PortalTimeline.tsx:56,122`), bouton de package Teams
  (carte Channels) ;
- **Images** via `<img src>` : avatars d'agents (`GeneralEditor.tsx:30`), vignettes de pièces jointes
  (`PortalTimeline.tsx:42,108`).

Les données / formulaires / mutations (qui passent par `fetch`) continuent de fonctionner. **Effet net en auth-ON :
images cassées + téléchargements en échec un peu partout dans l'UI, silencieusement.**

**Gravité :** ce n'est **pas une faille de sécu** (échoue *fermé* → 401, aucune fuite) — c'est un **bug
fonctionnel/UX du mode auth-activée**. Sévérité élevée *pour qui active l'auth sur le web* (typiquement une expo
publique via cloudflared/reverse-proxy avec auth on), nulle en déploiement par défaut (auth off). Ce n'est introduit
par aucune feature récente : tous ces points suivent le même pattern bare-`<a>`/`<img>` historique.

**Corrections possibles (app-wide — pas un patch ponctuel par feature) :**
1. **`fetch` authentifié → `URL.createObjectURL` (blob)** pour télécharger / alimenter les `<img>` : le JS récupère
   avec le header Bearer puis crée un object URL. Le plus propre, marche partout. À appliquer à *tous* les points.
2. **Cookie `HttpOnly`** pour le token → le navigateur l'enverrait automatiquement sur `<a>`/`<img>`. ⚠️ mais
   `server.rs:280` indique que les cookies ont été **désactivés exprès** (anti-CSRF) : rouvre une décision d'archi.
3. **Token de requête signé court** dans l'URL des ressources → met le token dans les URLs/logs : déconseillé.

→ À traiter dans le **chantier Auth + RBAC** ci-dessous, pas en correctif isolé.

## Améliorations in-app (roadmap — non planifié)

Si on veut de l'auth admin **native** (au lieu de déléguer au proxy) :
- authentification : login + sessions, idéalement **OIDC** (déléguer à l'IdP de l'entreprise) ;
- autorisation : **RBAC** pour opérateurs (lecture seule / admin / etc.) ;
- **TLS** natif (ou rester derrière proxy) ;
- **audit log** des actions admin ;
- fail-closed : warning/refus si exposé (`0.0.0.0`) sans auth ;
- **chargements de ressources authentifiés** (cf. bug ci-dessus) : faire passer downloads + images par `fetch`+blob
  (ou trancher la question cookie) pour que l'auth-ON ne casse plus l'UI.

C'est une **feature produit séparée**, **orthogonale à la branche mémoire SurrealDB** (ne bloque pas son merge) et
**distincte du user-scoping des mémoires** (utilisateurs des canaux de chat). À scoper le jour où on décide d'agir —
le bug auth-ON ci-dessus est le déclencheur concret qui justifie d'ouvrir ce chantier.

## Statut

**Documenté, non planifié.** Décision : on trace le constat, on déploie en attendant via tunnel SSH / reverse proxy,
et on décidera plus tard d'un éventuel chantier d'auth admin native.
