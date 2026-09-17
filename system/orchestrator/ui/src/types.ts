export interface Container {
  name: string;
  image: string;
  status: string;
  state: string;
  service: string | null;
  rollback_available: boolean;
  prev_container: string | null;
  operation: string | null;
  serving?: boolean;
  container_id?: string;
  image_id?: string;
}

export interface PsiWindow { avg10: number; avg60: number; avg300: number; }
export interface PsiResource { some: PsiWindow; full: PsiWindow | null; }
export interface Psi { cpu: PsiResource; memory: PsiResource; io: PsiResource; }
export interface Health { status: 'green' | 'yellow' | 'red'; reasons: string[]; }
export interface TopProcess {
  pid: number; comm: string; rss_mb: number; cpu_pct: number;
  container: string; state: string;
}
export interface HostMetrics {
  ts: number;
  health: Health;
  memory: {
    total_mb: number; available_mb: number; cached_mb: number; buffers_mb: number;
    swap_total_mb: number; swap_free_mb: number;
  };
  psi: Psi | null;
  oom_kills: number;
  top_processes: TopProcess[];
}
