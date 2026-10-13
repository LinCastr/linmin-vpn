# AGENTS.md — regras gerais para agentes de IA

Contrato de trabalho que vale para qualquer tarefa neste repositório. O que é
específico do projeto fica em `PROJETO.md`; o que já foi feito, os problemas
resolvidos e o roadmap ficam em `RESUMO.md` (local, fora do git). Leia os dois
antes da primeira edição.

@PROJETO.md
@RESUMO.md

Responda sempre em **português do Brasil**.

---

## 1. Como trabalhar

1. **Leia antes de escrever.** Abra o arquivo que vai mudar e a parte da
   documentação que fala dele antes de editar.
2. **Diga o diagnóstico antes de editar.** Em 3–6 linhas: o que entendeu do
   pedido, onde está a causa (arquivo e linha) e o que pretende mudar. Se o
   pedido tiver duas leituras, **pergunte** em vez de adivinhar.
3. **Escopo mínimo.** Mexa só no que o pedido cobre.
4. **Uma mudança por vez.** Edite, verifique, só então siga.
5. **Verifique de verdade** (como, está no `PROJETO.md`). Sem verificação, não
   diga que está pronto.
6. **Relate com honestidade:** o que mudou, o que testou e o que **não** testou.

## 2. Economia de tokens

- **Comece pelo `RESUMO.md`**, não pelo histórico da conversa.
- **Não leia arquivo grande inteiro.** Use `grep -n` e leia só o trecho.
- **Não releia** o que já está no contexto nem o arquivo que acabou de editar.
- Saída de comando: filtre (`| tail`, `| grep`, `| head`).
- **Ao terminar cada tarefa, atualize o `RESUMO.md`.**

## 3. Segurança e git

- **Idioma do repositório: inglês.** README e docs públicas, mensagens de commit,
  títulos/descrições de PR/issue, comentários e textos do código saem em inglês.
  Os arquivos de regras dos agentes (este, `PROJETO.md`, `RESUMO.md`) e a doc
  interna detalhada ficam em português. A conversa com o usuário é em pt-BR.
- **Não faça `git push`** sem o usuário pedir. Commit, sim; publicar, só a pedido.
- **Novos repositórios na organização:** todo repositório novo deve ser criado na organização **LinCastr** (`github.com/LinCastr`), nunca na conta pessoal.
- **Nunca comite** dados pessoais, tokens, senhas, nem caminhos com `/home/val47`.
  O IP 26.x da VPN, GUIDs de rede e nomes de peers são dados pessoais: não comite.
- **Não mate processo com `pkill -f <padrao>`:** casa com o próprio shell do
  agente (já aconteceu nesta sessão, exit 144). Mate por PID, ou `pkill -x <nome>`.
- Antes de apagar ou sobrescrever, olhe o alvo.

## 4. Comunicação

- Avise só antes dos passos importantes; não narre cada micro-passo.
- Se um comando, teste ou build falhar, relate os erros de forma objetiva.
- Comandos para o usuário rodar: um por bloco ```bash, sem `$` na frente.
