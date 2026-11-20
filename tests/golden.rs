//! F0: golden do Python normalizado — compara schema, não valores voláteis.
//!
//! `docs/golden-python-once.txt` é gerado por
//! `disk_usage.py --once` com tamanhos/percentuais/horários mascarados.
//! Este teste trava o schema: painéis, colunas e seções devem existir;
//! valores voláteis NÃO devem aparecer (viraram `<SIZE>`/`<PCT>`/`<TIME>`).

use std::path::PathBuf;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/golden-python-once.txt")
}

fn golden() -> String {
    std::fs::read_to_string(golden_path()).expect("docs/golden-python-once.txt existe (F0)")
}

#[test]
fn golden_has_all_panels() {
    let g = golden();
    for marker in [
        "Antigravity Disk Usage",
        "Diretório:",
        "Memória RAM",
        "Discos Locais",
        "Navegador de Diretórios",
        "FÍSICO",
        "NUVEM",
        "VIRTUAL",
    ] {
        assert!(g.contains(marker), "golden sem painel/coluna: {marker}");
    }
}

#[test]
fn golden_volatiles_are_masked() {
    let g = golden();
    let body: String = g
        .lines()
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("<SIZE>"),
        "tamanhos deveriam estar mascarados"
    );
    assert!(
        body.contains("<PCT>"),
        "percentuais deveriam estar mascarados"
    );
    assert!(body.contains("<TIME>"), "horário deveria estar mascarado");
}

#[test]
fn golden_documents_provenance() {
    let g = golden();
    assert!(
        g.contains("disk_usage.py --once"),
        "golden deve citar o comando gerador"
    );
}
