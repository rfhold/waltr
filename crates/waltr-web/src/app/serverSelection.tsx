import { createContext, useContext } from "solid-js";

export type ServerSelection = {
  switchServer: () => void;
  retry: () => void;
};

export const ServerSelectionContext = createContext<ServerSelection>();
export const useServerSelection = () => useContext(ServerSelectionContext);
