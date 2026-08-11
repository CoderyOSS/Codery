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
