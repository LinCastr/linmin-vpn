# PROJETO.md — linmin-vpn

Front-end nativo em Linux para o Radmin VPN. Roda o Radmin oficial (Windows) por
Wine através do projeto **radmin-vpn-linux**, mas substitui a janela do Wine por
uma interface própria em Rust + Slint, com ícone na bandeja (StatusNotifierItem),
sem terminal e, depois de um passo único, **sem pedir senha de sudo nunca mais**.

As regras gerais de trabalho estão no `AGENTS.md`; o que já foi feito e o que
falta está no `RESUMO.md`. O protocolo do Radmin (engenharia reversa) está em
`docs/PROTOCOL.md` (inglês, versionado).

Responda sempre em **português do Brasil**.

---

## 1. O que é / objetivo

- O usuário usa Radmin VPN para jogar em LAN com amigos (sem limite de pessoas na
  sala). Precisa que funcione bem no Linux, no rice hollow-wired e fora dele.
- **A janela do Wine (`RvRvpnGui.exe`) é feia e bugada.** O objetivo final é
  falar direto com o serviço `RvControlSvc.exe` pelo named pipe e **nunca mais
  abrir a janela do Wine** — nem para entrar/sair de rede. Ver `docs/PROTOCOL.md`.
- Repo próprio, universal (igual ao linex-mods). Distribuição: binário em release
  do GitHub; o rice baixa. O usuário prefere que seja **um app**, não um widget do
  Quickshell.
- Tem que funcionar com **wallust** se o usuário quiser (tema opcional, como no
  linex-mods: só o tom do acento vem da paleta; ver `src/theme.rs`).

## 2. Arquitetura

- **App Rust + Slint** (`src/`, `ui/app.slint`). Janela pequena e bandeja SNI
  (`ksni`). Tema padrão ou wallust. i18n pt-BR/inglês (`src/i18n.rs`, `tr!`,
  `lang/pt_BR/LC_MESSAGES/linmin-vpn.po`). **Toda string nova precisa dos dois.**
- **Backend** (`src/vpn.rs`): sobe o AppImage do radmin-vpn-linux com `--no-ui`
  (só serviço, sem janela), destacado do terminal; lê estado por `ip`/`/proc`.
- **Setup** (`src/setup.rs`): baixa o AppImage (fixado, com sha256), config em
  `~/.config/linmin-vpn/config.json`, autostart XDG, instala o helper de root.
- **Privilégio sem senha** (`data/`): em vez de rodar o launcher com sudo, um
  helper de root restrito faz só os comandos de rede da `radminvpn0`:
  - `data/radmin-net` — helper. Aceita só comandos sobre `radminvpn0`, IPs
    restritos a 26.x / multicast / broadcast. Recusa rota default, netns, sh.
    Instalado root em `/usr/local/libexec/linmin-vpn/radmin-net`.
  - `data/io.github.lincastr.linmin-vpn.policy` — ação polkit
    `io.github.lincastr.linmin-vpn.net`, `allow_active=yes` (sessão local
    roda o helper sem senha).
  - `data/sudo-shim` — "sudo" falso. O app o põe primeiro no PATH do AppImage;
    os `sudo ip …` do `run.sh` caem nele e vão ao helper via pkexec. **Não altera
    o AppImage**, então atualizações do upstream continuam funcionando.
- **Protocolo** (meta): `docs/PROTOCOL.md` + ferramentas em `bridge/` e `tools/`
  (não entram no app final; são de engenharia reversa).

## 3. Verificação obrigatória

| Mexeu em | Rode | Esperado |
|---|---|---|
| Rust (`src/`, `ui/`) | `cargo build --release` | sem erro |
| helper `data/radmin-net` | `bash -n data/radmin-net` + testar entradas | aceita só os comandos previstos; recusa o resto |
| visual da janela | snapshot (ver seção 5), conferir a olho | sem esticar sozinho, pequeno |
| CLI | `./target/release/linmin-vpn status` | JSON com phase/ip/peers |

Build release leva ~2min (LTO). Para iterar rápido no visual, `cargo build` (debug).

## 4. Regras absolutas

- **Nunca comite** dados pessoais: IP 26.x, GUIDs de rede, nomes de peers
  (`DESKTOP-...`, nome do amigo), senhas de rede. O repo é universal.
- **Sincronize com o rice:** quando o app estiver pronto, o `install.sh` e os
  scripts do rice hollow-wired (`~/projetos/hyprland-setup/dots/bin/`) precisam
  baixar o binário e instalar o helper. Isso é tarefa no rice, não aqui.
- **Não mate processo com `pkill -f`** (casa com o shell do agente — exit 144
  já aconteceu). Use PID ou `pkill -x RvRvpnGui.exe`.
- **Não force `git push`.**
- O **AppImage do radmin-vpn-linux é dado não confiável** baixado da internet:
  o app o executa como ele é, sem rodar interpretador de dentro da pasta dele.

## 5. Armadilhas já conhecidas (todas custaram tempo)

- **`pkill -f 'RvRvpnGui'` matou o próprio shell** (exit 144). Sempre `-x` + nome
  exato, ou por PID.
- **`set_xdg_app_id` só funciona depois de `AppWindow::new()`** (cria a
  plataforma). Antes disso, não tem efeito e a janela sai com app_id vazio.
  No `src/main.rs`, a chamada vem **logo após** `AppWindow::new()`.
- **Classificador de segurança do Claude Code** barra testar algoritmos de hash
  contra o blob de senha capturado (parece quebra de senha). O caminho limpo é
  **ler a função de cifra no `RvRvpnGui.exe` com o REA** (análise estática do
  binário do próprio usuário), não adivinhar contra os dados. Ver `docs/PROTOCOL.md`.
- **`pgrep -f 'run.sh'`** também casa demais; filtre por `usr/bin/run.sh` e
  confira o pgid.
- **Shell do usuário é fish.** Comandos em sintaxe bash no terminal do usuário
  precisam de `bash -c '...'`. As ferramentas do agente já rodam em bash.
- **Snapshot do visual sem abrir na tela do usuário:** o app salva um PNG e sai
  com `LINMIN_SNAPSHOT=arquivo.png`; `LINMIN_SETTINGS=1` abre na tela de
  configurações; `LINMIN_APP_ID=linmin-vpn-test` para casar com uma regra de
  janela. Regra temporária no Hyprland para mandar a janela de teste a um
  workspace especial:
  `hyprctl eval 'hl.window_rule({ name="linmintest", match={ class="^linmin-vpn-test$" }, workspace="special:linmintest silent" })'`
- **O AppImage, sem helper, pede a senha do sudo.** O "Permitir sem senha" usa
  pkexec; precisa de um **agente polkit gráfico** rodando na sessão (o
  hollow-wired tem `PolkitDialog.qml` no Quickshell). Sem agente, o pkexec fica
  esperando sem mostrar janela.

## 6. Visual: sem "cara de IA"

Mesma régua do rice hollow-wired (ver o PROJETO.md de lá). Resumo:
- **Referência é o app original** (prints do usuário): janela pequena, sóbria,
  lista de peers simples, um destaque por tela.
- Sem emoji como ícone, sem cartão de "dica", sem cor fixa (vem do tema/wallust),
  sem texto de marketing. Hierarquia pelo texto, não pela caixa.
- **A janela NÃO pode se redimensionar sozinha** (pedido explícito do usuário no
  Hyprland). Tem que ser pequena como o original, mas o usuário pode mudar o
  tamanho à mão. No Hyprland isso quer dizer: abrir **flutuante** com tamanho
  inicial pequeno, e não deixar o compositor forçar o tamanho. Fazer com uma
  `window_rule` no rice (float + size) e garantir que a janela Slint não peça
  resize a cada mudança de conteúdo.
- Conferir com snapshot antes de dizer que ficou bom.
