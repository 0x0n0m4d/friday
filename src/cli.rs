//! Argumentos de linha de comando.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "sexta-feira",
    about = "Cliente ACP com TUI e push-to-talk para o agente Sexta-feira (ZeroClaw)",
    version
)]
pub struct Args {
    /// Apelido do agente no daemon ZeroClaw (obrigatório, exceto com --find-key).
    #[arg(long)]
    pub agent: Option<String>,

    /// Caminho do arquivo de configuração (padrão: XDG config).
    #[arg(long, value_name = "CAMINHO")]
    pub config: Option<String>,

    /// Começar com uma sessão nova, ignorando a sessão salva.
    #[arg(long = "new-session")]
    pub new_session: bool,

    /// Descobre o dispositivo e o código da tecla para o push-to-talk:
    /// lê UMA tecla em /dev/input/event*, imprime e sai.
    #[arg(long = "find-key", value_name = "DISPOSITIVO", num_args = 0..=1,
          default_missing_value = "")]
    pub find_key: Option<String>,

    /// Desativa o modo de texto (TUI) e só valida a configuração.
    #[arg(long = "check-config")]
    pub check_config: bool,

    /// Modo verboso: erros e caminhos silenciosos ("de praxe") aparecem na
    /// tela marcados como [diag]. Tudo também é registrado no journal
    /// mesmo sem esta opção.
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cmdline_valida() {
        Args::command().debug_assert();
    }

    #[test]
    fn exige_agente_em_uso_normal() {
        let args = Args::try_parse_from(["sexta-feira"]).unwrap();
        assert!(args.agent.is_none());
        assert!(!args.new_session);
    }
}
