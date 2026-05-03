export interface Container {
  name: string;
  image: string;
  status: string;
  state: string;
  service: string | null;
  rollback_available: boolean;
  prev_container: string | null;
  operation: string | null;
}
